//! The two transport properties both keyed voice backends must have.
//!
//! [`super::remote`] resolves intent and [`super::transcribe`] turns audio into
//! text; they share no envelope, no endpoint and no credential header, and they
//! do share exactly these two things — so they live here rather than twice, and
//! a third keyed backend gets them by construction.
//!
//! # 1. A redirect is never followed
//!
//! `reqwest::Client::new()` follows up to ten redirects, and on a cross-origin
//! hop it removes `Authorization`, `Cookie`, `Cookie2`, `Proxy-Authorization`
//! and `WWW-Authenticate` — read from `reqwest` 0.13.5's
//! `redirect::remove_sensitive_headers`, which is the whole list. **It does not
//! remove a custom header**, and the Anthropic intent backend authenticates
//! with `x-api-key`. So a redirect from the configured endpoint to another
//! HTTPS origin carried the user's key to that origin, and that host's error
//! body would then have been quoted back into the webview by the API-error
//! path.
//!
//! The transcription backend uses the standard `Authorization` header, which
//! reqwest does strip — but a 307 or 308 re-sends the **body**, and that body is
//! the user's voice. Both backends get the same policy for that reason, not
//! only the one with the custom header.
//!
//! `redirect::Policy::none()` rather than a same-origin allowlist because
//! neither API redirects, so an allowlist would be machinery for a case that
//! does not arise. A backend whose endpoint moves gets a settings change, which
//! is a thing the user can see, rather than a silent hop.
//!
//! **[`client`] returns an `Option` and the callers fail closed.** The obvious
//! spelling — `builder().redirect(…).build().unwrap_or_default()` or
//! `.unwrap_or_else(|_| Client::new())` — is a trap here: the first silently
//! yields a client with no policy at all and the second calls a constructor
//! documented to panic on exactly the failure it is handling. A backend with no
//! client says so in a sentence.
//!
//! # 2. A response body is bounded before it is parsed
//!
//! `Response::json` collects the whole body and then deserialises it. A request
//! timeout bounds elapsed time, not bytes, so a compromised or confused
//! endpoint could stream a large body well inside the timeout and exhaust the
//! desktop process before anything validated the answer. `max_tokens` bounds a
//! normal successful generation and bounds nothing about a proxy response or an
//! error body.
//!
//! [`capped_body`] applies the bound **while reading**, and therefore before
//! any UTF-8 conversion or deserialisation, for successful and non-success
//! replies alike — a non-2xx body is read by the same path, since the error
//! detail is quoted from it.
//!
//! Both properties are PRD #802's landed-work security audit.

/// How much of a response body either backend will read.
///
/// The replies are small by construction — a tool-use block of a few dozen
/// tokens, or a transcript of at most thirty seconds of speech — so this is
/// generous by three orders of magnitude and still refuses a flood. It is the
/// same number as [`super::prompt::MAX_SCAN_BYTES`] and for the same reason:
/// past the point where an answer could plausibly be, more bytes are only more
/// allocation.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Why a body did not come back whole.
///
/// Two variants because they mean different things to the sentence a backend
/// renders: [`Self::TooLarge`] is this app refusing, and [`Self::Transport`] is
/// the connection failing mid-body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The response crossed [`MAX_BODY_BYTES`] and was abandoned.
    TooLarge,
    /// The body stopped arriving.
    Transport,
}

/// An HTTPS client that never follows a redirect, or `None` when one cannot be
/// built.
///
/// `None` is a fail-closed answer and not a fallback: see the module doc for
/// why the two obvious fallbacks are both worse than a sentence.
pub fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
}

/// Read at most `cap` bytes of `response`'s body, refusing rather than
/// allocating past it.
///
/// Exactly `cap` bytes is a success — the cap is the size this app is willing
/// to parse, so a body that fills it exactly is still parseable. `cap + 1` is
/// [`BodyError::TooLarge`], reported before anything looks at the bytes.
///
/// A declared `Content-Length` over the cap short-circuits before the first
/// chunk is read, which costs nothing and is the common shape of the case this
/// exists for. A body with no declared length, or a lying one, is still bounded
/// by the loop.
pub async fn capped_body(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut collected: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if collected.len() + chunk.len() > cap {
                    return Err(BodyError::TooLarge);
                }
                collected.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(collected),
            Err(_) => return Err(BodyError::Transport),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `reqwest::Response` built from an `http::Response`, which is how both
    /// halves of [`capped_body`] are driven with **no socket** — PRD #802 M5's
    /// rule that nothing in the merge-blocking tier opens one.
    fn response(status: u16, body: Vec<u8>) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .status(status)
                .body(body)
                .expect("a response"),
        )
    }

    /// The redirect policy, asserted through the client's own `Debug` — which
    /// prints `redirect_policy` only when it is **not** the default, so this
    /// distinguishes a `Policy::none()` client from the `Client::new()` one
    /// that carried the finding.
    ///
    /// No socket, per the rule this module's callers already follow: reqwest
    /// exposes no way to construct a redirect `Attempt`, so the alternative
    /// would be a loopback server, and what that would test is reqwest.
    #[test]
    fn voice_http_client_never_follows_a_redirect() {
        let built = client().expect("a client");
        let rendered = format!("{built:?}");
        assert!(
            rendered.contains(r#"redirect_policy: "Policy(None)""#),
            "the client follows redirects: {rendered}"
        );
        // The shape the finding had, for contrast: a default client prints no
        // redirect_policy field at all, because ten-hop following is default.
        let default = reqwest::Client::new();
        assert!(
            !format!("{default:?}").contains("redirect_policy"),
            "reqwest's default is no longer the one this test contrasts with"
        );
    }

    #[tokio::test]
    async fn voice_http_reads_exactly_the_cap_and_refuses_one_byte_more() {
        // Success, exactly at the limit.
        let body = vec![b'x'; MAX_BODY_BYTES];
        let read = capped_body(response(200, body), MAX_BODY_BYTES)
            .await
            .expect("exactly the cap is readable");
        assert_eq!(read.len(), MAX_BODY_BYTES);

        // Refused, one byte over.
        let body = vec![b'x'; MAX_BODY_BYTES + 1];
        assert_eq!(
            capped_body(response(200, body), MAX_BODY_BYTES).await,
            Err(BodyError::TooLarge)
        );
    }

    #[tokio::test]
    async fn voice_http_bounds_a_non_success_body_too() {
        // The error path reads the body as well — it quotes the API's own
        // message — so the bound has to apply there or it applies to the half
        // an attacker does not control.
        let body = vec![b'x'; MAX_BODY_BYTES];
        let read = capped_body(response(500, body), MAX_BODY_BYTES)
            .await
            .expect("exactly the cap is readable");
        assert_eq!(read.len(), MAX_BODY_BYTES);

        let body = vec![b'x'; MAX_BODY_BYTES + 1];
        assert_eq!(
            capped_body(response(429, body), MAX_BODY_BYTES).await,
            Err(BodyError::TooLarge)
        );
    }

    #[tokio::test]
    async fn voice_http_refuses_a_declared_length_over_the_cap_before_reading() {
        // `http::Response::builder().body(Vec<u8>)` gives reqwest a known
        // length, so this is the short-circuit rather than the loop.
        let body = vec![b'x'; MAX_BODY_BYTES * 4];
        let over = response(200, body);
        assert_eq!(over.content_length(), Some((MAX_BODY_BYTES * 4) as u64));
        assert_eq!(
            capped_body(over, MAX_BODY_BYTES).await,
            Err(BodyError::TooLarge)
        );
    }

    #[tokio::test]
    async fn voice_http_reads_a_small_body_whole() {
        let read = capped_body(response(200, b"{\"ok\":true}".to_vec()), MAX_BODY_BYTES)
            .await
            .expect("reads");
        assert_eq!(read, b"{\"ok\":true}");
    }
}
