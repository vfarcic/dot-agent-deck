#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Create a dispatcher through the real TUI using a wrapped Codex stand-in with no Codex trust helper on PATH. Before fetching, the pane may receive only fixed doorbells; after Idle it fetches and visibly renders exactly one seed without typed seed content.
#[spec("codex/wrap/007")]
#[test]
fn codex_wrap_007_untrusted_codex_pane_receives_its_seed_once() {
    route::client_seed(7);
}
