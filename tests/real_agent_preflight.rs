//! Fast-tier coverage for the real-agent preflight gate the e2e tier skips on.
//!
//! `common::check_claude_available` decides whether a real-Claude scenario runs
//! or is skipped, and it does so **offline** — no probe request, because a live
//! round trip would spend tokens on every e2e run. That makes the credential
//! shapes it accepts and rejects the whole contract, so they are asserted here
//! rather than argued: the pure `claude_oauth_usable` half is called directly
//! with a synthetic `claudeAiOauth` object and a fixed clock. The filesystem
//! half (CLI on PATH, regular file, JSON parse) is exercised by the e2e tier
//! itself and is not re-modelled here.
//!
//! PRD #126 audit follow-up: the shapes that motivated this file are the
//! ASYMMETRIC ones — a set carrying only one of the two tokens. Each expiry must
//! be bound to the presence of its own token, or the absent half's "no expiry
//! information" silently votes "live" for a token that is not there.
//!
//! Issue #1731 narrowed the rule: a test HOME gets an ACCESS-ONLY copy of the
//! login, so a refresh token no longer rescues an expired access token, and the
//! access token must outlast the harness's run margin (one hour).

use serde_json::{Value, json};

dot_agent_deck_test_harness::link!();

/// Fixed clock for every case below; nothing here reads the wall clock.
const NOW_MS: i64 = 1_700_000_000_000;
const PAST_MS: i64 = NOW_MS - 60_000;
/// Two hours ahead: past the harness's one-hour run margin.
const FUTURE_MS: i64 = NOW_MS + 2 * 3_600_000;
/// Ten minutes ahead: live, but inside the run margin.
const SOON_MS: i64 = NOW_MS + 600_000;

fn usable(oauth: &Value) -> Result<(), String> {
    common::claude_oauth_usable(oauth, NOW_MS)
}

#[test]
fn live_access_token_alone_is_usable() {
    let oauth = json!({ "accessToken": "at", "expiresAt": FUTURE_MS });
    assert_eq!(
        usable(&oauth),
        Ok(()),
        "a live access token is usable on its own; a login that has not yet \
         produced a refresh token must not be reported as unusable"
    );
}

/// Issue #1731: a refresh token beside a lasting access token changes nothing —
/// it is not copied, so it is not consulted.
#[test]
fn a_lasting_access_token_with_a_refresh_token_is_usable() {
    let oauth = json!({
        "accessToken": "at",
        "expiresAt": FUTURE_MS,
        "refreshToken": "rt",
        "refreshTokenExpiresAt": FUTURE_MS,
    });
    assert_eq!(usable(&oauth), Ok(()));
}

/// Issue #1731: with no `expiresAt` the harness cannot tell whether the
/// access-only copy lasts the run, so it refuses rather than guess (the rule
/// PRD #1666 gave the Pi login). It used to read as "no expiry information,
/// never expired".
#[test]
fn an_access_token_without_an_expiry_is_rejected() {
    let oauth = json!({ "accessToken": "at", "refreshToken": "rt" });
    let error = usable(&oauth).expect_err("an access token of unknown lifetime is refused");
    assert!(error.contains("no `expiresAt`"), "{error}");
    assert!(
        error.contains("claude login"),
        "the refusal names the remedy: {error}"
    );
}

/// Issue #1731 removed PRD #126's deliberate acceptance of this shape. Claude
/// Code on the host refreshes from it; the access-only copy a test HOME gets
/// cannot, so it would fail at its first request.
#[test]
fn expired_access_token_with_a_live_refresh_token_is_rejected() {
    let oauth = json!({
        "accessToken": "at",
        "expiresAt": PAST_MS,
        "refreshToken": "rt",
        "refreshTokenExpiresAt": FUTURE_MS,
    });
    let error = usable(&oauth)
        .expect_err("the test copy carries no refresh token, so an expired access token is spent");
    assert!(error.contains("has expired"), "{error}");
    assert!(
        error.contains("claude login"),
        "the refusal names the remedy: {error}"
    );
}

/// Issue #1731: a token that is live now but would expire during the run is
/// refused with how long it has left and the remedy, quoting no token.
#[test]
fn an_access_token_expiring_within_the_run_margin_is_rejected() {
    let oauth = json!({
        "accessToken": "secret-at",
        "expiresAt": SOON_MS,
        "refreshToken": "secret-rt",
        "refreshTokenExpiresAt": FUTURE_MS,
    });
    let error = usable(&oauth).expect_err("ten minutes does not last a test run");
    assert!(error.contains("expires in 10 min"), "{error}");
    assert!(
        error.contains("60 min"),
        "the refusal names the margin: {error}"
    );
    assert!(
        error.contains("claude login"),
        "the refusal names the remedy: {error}"
    );
    assert!(
        !error.contains("secret"),
        "the refusal quotes no token: {error}"
    );
}

/// Asymmetric shape A (PRD #126 audit): the sole token is an EXPIRED access
/// token and there is no refresh token to renew it.
#[test]
fn expired_sole_access_token_with_no_refresh_token_is_rejected() {
    let oauth = json!({ "accessToken": "at", "expiresAt": PAST_MS });
    let error = usable(&oauth).expect_err(
        "an expired access token with NO refresh token cannot be refreshed and must be rejected",
    );
    assert!(
        error.contains("has expired"),
        "the rejection must name expiry, not a missing token: {error}"
    );
}

/// Asymmetric shape B (PRD #126 audit): no access token at all. Since issue
/// #1731 a refresh token alone is never enough, live or spent: the copy does
/// not carry it.
#[test]
fn a_sole_refresh_token_with_no_access_token_is_rejected() {
    for expiry in [PAST_MS, FUTURE_MS] {
        let oauth = json!({ "refreshToken": "rt", "refreshTokenExpiresAt": expiry });
        let error = usable(&oauth).expect_err("a refresh token alone leaves the copy nothing");
        assert!(
            error.contains("carry no access token, only a refresh token"),
            "the rejection must name the missing access token: {error}"
        );
    }
}

/// The same asymmetry through the EMPTY-STRING door: a present-but-empty token
/// is no token, so its own live expiry must not rescue the set.
#[test]
fn an_empty_token_string_does_not_borrow_its_own_live_expiry() {
    let oauth = json!({ "accessToken": "", "expiresAt": FUTURE_MS });
    let error = usable(&oauth).expect_err("an empty access token is no access token");
    assert!(
        error.contains("no access or refresh token"),
        "an empty token must count as missing: {error}"
    );
}

#[test]
fn both_tokens_expired_is_rejected() {
    let oauth = json!({
        "accessToken": "at",
        "expiresAt": PAST_MS,
        "refreshToken": "rt",
        "refreshTokenExpiresAt": PAST_MS,
    });
    let error =
        usable(&oauth).expect_err("a fully spent credential set must be reported as unusable");
    assert!(
        error.contains("has expired"),
        "the rejection must name expiry: {error}"
    );
}

/// No tokens at all keeps its own, more specific message — the operator has
/// never logged in rather than let a login lapse.
#[test]
fn no_tokens_at_all_is_rejected_as_missing_rather_than_expired() {
    let oauth = json!({ "expiresAt": FUTURE_MS, "refreshTokenExpiresAt": FUTURE_MS });
    let error = usable(&oauth).expect_err("a credential set with no tokens is unusable");
    assert!(
        error.contains("no access or refresh token"),
        "a token-less set must be reported as missing, not as expired: {error}"
    );
}
