#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Create a dispatcher through the real TUI using a wrapped Codex stand-in with no Codex trust helper on PATH. Exactly one seed waits in its inbox and is fetched after Idle without typed seed bytes or a typed retry.
#[spec("codex/wrap/007")]
#[test]
fn codex_wrap_007_untrusted_codex_pane_receives_its_seed_once() {
    route::client_seed(7);
}
