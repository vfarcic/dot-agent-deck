#![cfg(all(feature = "e2e", unix))]

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/inbox_route.rs"]
mod route;

use spec::spec;

/// Scenario: Create a dispatcher through the real TUI using a wrapped Codex stand-in with no Codex trust helper on PATH. The deck types nothing into a Codex pane, before or after its Idle, and the seed waits queued; when its model runs `inbox fetch` on its own, it visibly renders exactly one seed without typed seed content.
#[spec("codex/wrap/007")]
#[test]
fn codex_wrap_007_untrusted_codex_pane_gets_no_typed_seed_and_fetches_it_once() {
    route::client_seed(7);
}
