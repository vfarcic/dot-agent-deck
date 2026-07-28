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

use serde_json::{Value, json};

mod common;

/// Fixed clock for every case below; nothing here reads the wall clock.
const NOW_MS: i64 = 1_700_000_000_000;
const PAST_MS: i64 = NOW_MS - 60_000;
const FUTURE_MS: i64 = NOW_MS + 60_000;

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

#[test]
fn tokens_without_expiry_fields_are_usable() {
    let oauth = json!({ "accessToken": "at", "refreshToken": "rt" });
    assert_eq!(
        usable(&oauth),
        Ok(()),
        "an absent expiry means NO EXPIRY INFORMATION, never expired"
    );
}

/// The deliberate decision this check preserves: Claude Code itself refreshes an
/// expired access token from a live refresh token, so that shape is usable.
#[test]
fn expired_access_token_with_a_live_refresh_token_is_usable() {
    let oauth = json!({
        "accessToken": "at",
        "expiresAt": PAST_MS,
        "refreshToken": "rt",
        "refreshTokenExpiresAt": FUTURE_MS,
    });
    assert_eq!(
        usable(&oauth),
        Ok(()),
        "an expired access token backed by a LIVE refresh token still works, \
         because the CLI refreshes it"
    );
}

/// Asymmetric shape A (PRD #126 audit): the sole token is an EXPIRED access
/// token and there is no refresh token to renew it. Before each expiry was bound
/// to its own token this passed, because the absent `refreshTokenExpiresAt`
/// counted as "no expiry information" — i.e. as a live refresh token that did
/// not exist.
#[test]
fn expired_sole_access_token_with_no_refresh_token_is_rejected() {
    let oauth = json!({ "accessToken": "at", "expiresAt": PAST_MS });
    let error = usable(&oauth).expect_err(
        "an expired access token with NO refresh token cannot be refreshed and must be rejected",
    );
    assert!(
        error.contains("expired and cannot be refreshed"),
        "the rejection must name expiry, not a missing token: {error}"
    );
}

/// Asymmetric shape B (PRD #126 audit): the converse — no access token at all,
/// and the refresh token that would have to produce one is itself spent. The
/// absent `expiresAt` used to vote "live" for the access token that is missing.
#[test]
fn expired_sole_refresh_token_with_no_access_token_is_rejected() {
    let oauth = json!({ "refreshToken": "rt", "refreshTokenExpiresAt": PAST_MS });
    let error = usable(&oauth).expect_err(
        "an expired refresh token with no access token leaves nothing usable and must be rejected",
    );
    assert!(
        error.contains("expired and cannot be refreshed"),
        "the rejection must name expiry, not a missing token: {error}"
    );
}

/// The same asymmetry through the EMPTY-STRING door: a present-but-empty token
/// is no token, so its own live expiry must not rescue the set.
#[test]
fn an_empty_token_string_does_not_borrow_its_own_live_expiry() {
    let oauth = json!({
        "accessToken": "at",
        "expiresAt": PAST_MS,
        "refreshToken": "",
        "refreshTokenExpiresAt": FUTURE_MS,
    });
    let error = usable(&oauth)
        .expect_err("an empty refresh token cannot refresh anything, however live its expiry is");
    assert!(
        error.contains("expired and cannot be refreshed"),
        "the rejection must name expiry, not a missing token: {error}"
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
        error.contains("expired and cannot be refreshed"),
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
