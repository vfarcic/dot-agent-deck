//! L1 widget / layout snapshot tests for the experimental feature-flag
//! gated surface (PRD #139).
//!
//! Per PRD #77 Decision 2 these are in-process tests using ratatui's
//! `TestBackend` plus `insta` file snapshots — no subprocess, no PTY.
//! They mirror `tests/render_dashboard.rs` and `tests/render_keybindings.rs`:
//! build an in-memory `Features` value, render the gated footer into a
//! `Buffer`, and snapshot the stringified buffer.
//!
//! PRD #139 ships flag PLUMBING plus ONE throwaway gated surface for
//! end-to-end validation (Open Question 5 / M4.1): a dashboard footer
//! label rendering the exact text `experimental: on` ONLY when the
//! `experimental` flag is enabled, and nothing when it is off. The
//! surface is gated behind the wrapper `features::show_experimental_footer()`
//! (the production call-site predicate, which reads the per-process
//! shared `Features`).
//!
//! These tests were authored RED: the `dot_agent_deck::features` module
//! and the `dot_agent_deck::ui::render_experimental_footer_to_buffer`
//! seam do not exist yet. They go GREEN once the coder implements them to
//! match the contract referenced here.
//!
//! INJECTION SEAM (chosen to match every existing L1 render seam, which
//! takes its state as a by-reference parameter — `render_stats_bar_to_buffer(&stats, …)`,
//! `render_card_to_buffer(&session, …)`, `render_help_overlay_with_bindings_to_buffer(&config, …)`):
//! the gated footer is rendered by a standalone seam
//! `render_experimental_footer_to_buffer(features: &Features, width, height)`
//! that observes the passed `&Features` value. The reload test additionally
//! exercises the production global wrapper `show_experimental_footer()`
//! to prove live re-evaluation after a synthetic config change.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};

use dot_agent_deck::event::AgentType;
use dot_agent_deck::features::{self, Features};
use dot_agent_deck::state::{SessionState, SessionStatus};
use dot_agent_deck::ui::{
    CardDensityKind, render_card_to_buffer, render_experimental_footer_to_buffer,
};
use spec::spec;

/// Exact text the throwaway gated footer renders when the experimental
/// flag is ON (PRD #139 Open Question 5 / M4.1). Mirrored here so a drift
/// between the production label and the test surfaces as a failed
/// `contains` assertion rather than a silent snapshot churn.
const EXPERIMENTAL_FOOTER_TEXT: &str = "experimental: on";

/// Width at which `render_session_card` flips to its wide layout — mirrors the
/// same constant in `tests/render_dashboard.rs` so the Pi-card render in
/// `features/gating/004` uses the identical height math as `dashboard/pane/007`.
const RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH: u16 = 60;

/// Serializes the tests that mutate the *process-global* `Features`
/// (`features/reload/001` and `features/gating/004`) so a concurrent flip can't
/// bleed across another test's render/assert window. Under plain `cargo test`
/// (CI) the tests in this binary share one process and run on threads;
/// `cargo test-fast`/nextest isolates each test in its own process, where this
/// is belt-and-suspenders. The by-value `&Features` render seam used by
/// `features/gating/001-002` never reads the global, so those need no lock.
static FLAG_LOCK: Mutex<()> = Mutex::new(());

/// Stringify the rendered buffer — one line per row, cells joined into the
/// symbol layer — so `insta` diffs read like the rendered widget itself.
/// Mirrors the same helper in `tests/render_dashboard.rs` and
/// `tests/render_keybindings.rs`.
fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Scenario: Build a `Features` value with the experimental flag forced ON
/// via `Features::test_with(true)`, render the throwaway gated dashboard
/// footer into a `TestBackend` buffer at 80×1, and snapshot it. The
/// rendered footer must contain the exact label `experimental: on` —
/// proving the gated surface is visible when the flag is enabled (PRD #139
/// M4.1, ON path).
#[spec("features/gating/001")]
#[test]
fn gating_001_footer_visible_when_flag_on() {
    // PRD #139 catalog: features/gating/001 — flag forced ON makes the
    // throwaway footer label render. The flag is observed by passing a
    // `&Features` into the render seam (matching every existing L1 seam).
    let features = Features::test_with(true);
    let width: u16 = 80;
    let height: u16 = 1;
    let buffer = render_experimental_footer_to_buffer(&features, width, height);

    let text = buffer_to_text(&buffer);
    assert!(
        text.contains(EXPERIMENTAL_FOOTER_TEXT),
        "experimental flag is ON, so the dashboard footer must render \
         {EXPERIMENTAL_FOOTER_TEXT:?}; rendered footer was:\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Build a `Features` value with the experimental flag forced OFF
/// via `Features::test_with(false)`, render the gated footer into a
/// `TestBackend` buffer at 80×1, and snapshot it. With the flag off the
/// footer must be ENTIRELY ABSENT — the rendered buffer carries no
/// `experimental` text and is the blank pre-feature baseline, identical to
/// how the dashboard footer region looked before this surface existed (PRD
/// #139 M4.1, OFF path).
#[spec("features/gating/002")]
#[test]
fn gating_002_footer_hidden_when_flag_off() {
    // PRD #139 catalog: features/gating/002 — flag forced OFF hides the
    // surface entirely; the rendered region equals the pre-feature blank
    // baseline (no `experimental` text anywhere in the buffer).
    let features = Features::test_with(false);
    let width: u16 = 80;
    let height: u16 = 1;
    let buffer = render_experimental_footer_to_buffer(&features, width, height);

    let text = buffer_to_text(&buffer);
    assert!(
        !text.contains("experimental"),
        "experimental flag is OFF, so the footer must be hidden entirely \
         (no `experimental` text); rendered footer was:\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Model PRD #139 M2.2 live reload in-process. A shared
/// `Arc<RwLock<Features>>` (the M1.2 per-process shared value) starts with
/// the flag OFF; the first render shows no footer and the production
/// wrapper `features::show_experimental_footer()` reports hidden. Then a
/// SYNTHETIC `.dot-agent-deck.toml` change flips `experimental` -> true —
/// modeled by writing the new `Features` into the shared value and applying
/// it via `features::set_for_test(..)` (the watcher's apply step). With NO
/// process restart, the wrapper now reports visible and the next render
/// shows the `experimental: on` footer.
#[spec("features/reload/001")]
#[test]
fn reload_001_footer_appears_after_synthetic_config_change() {
    // PRD #139 catalog: features/reload/001 — a synthetic config-file
    // change flips the flag and the next render re-evaluates the wrapper,
    // surfacing the footer with no restart (in-process TestBackend +
    // synthetic file event). nextest runs each test in its own process, so
    // mutating the process-global `Features` here cannot leak into the
    // gating tests above.
    let width: u16 = 80;
    let height: u16 = 1;

    // Serialize with `features/gating/004`: both mutate the process-global
    // `Features`, so under plain `cargo test` (shared process/threads) their
    // set→read windows must not interleave (see FLAG_LOCK).
    let _flag_lock = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // M1.2: a single shared Features value per process. Startup default is
    // OFF (experimental = false). Install it as the process-global the
    // production wrapper reads.
    let shared: Arc<RwLock<Features>> = Arc::new(RwLock::new(Features::test_with(false)));
    features::set_for_test(*shared.read().unwrap());

    // First render cycle: flag OFF -> wrapper hidden, footer absent.
    assert!(
        !features::show_experimental_footer(),
        "wrapper must report hidden while the shared flag is OFF at startup"
    );
    let off_view = *shared.read().unwrap();
    let before = render_experimental_footer_to_buffer(&off_view, width, height);
    assert!(
        !buffer_to_text(&before).contains("experimental"),
        "footer must be absent before the synthetic config change"
    );

    // SYNTHETIC `.dot-agent-deck.toml` change (PRD #139 M2.1/M2.2): the file
    // watcher re-parses the [features] table and flips experimental ->
    // true, updating the shared value in place with NO process restart.
    // set_for_test models the watcher's apply step.
    *shared.write().unwrap() = Features::test_with(true);
    features::set_for_test(*shared.read().unwrap());

    // Next render cycle re-evaluates the wrapper: now visible.
    assert!(
        features::show_experimental_footer(),
        "wrapper must re-evaluate to visible after the synthetic file event (no restart)"
    );
    let on_view = *shared.read().unwrap();
    let after = render_experimental_footer_to_buffer(&on_view, width, height);
    assert!(
        buffer_to_text(&after).contains(EXPERIMENTAL_FOOTER_TEXT),
        "footer must show {EXPERIMENTAL_FOOTER_TEXT:?} on the next render after the flag flips on"
    );
}

/// Scenario: Build a live `AgentType::Pi` session with no display name (the
/// same fixture as `dashboard/pane/007`) and render its dashboard card twice.
/// With the experimental flag forced OFF, the card must NOT show the Pi
/// first-class identity (`Pi · …`) — it falls back to the pre-feature
/// unrecognized-agent baseline a `command = "pi"` pane showed before this PRD —
/// yet the card/pane stays VISIBLE (its session id `orch-01` still renders), so
/// the flag never hides an already-running pane. With the flag forced ON, the
/// same card shows the Pi agent-type identity `Pi · orch-01`. This pins the
/// `experimental`-flag gate of the Pi render surface (PRD #201 M5.1).
#[spec("features/gating/004")]
#[test]
fn gating_004_pi_card_identity_gated_by_flag() {
    // PRD #201 catalog: features/gating/004 — the Pi card's first-class
    // identity/status affordance is gated behind `features::show_pi_agent()`
    // at the render seam (CLAUDE.md #9). RED today: the card renderer does not
    // yet consult the flag, so the OFF render still shows `Pi · orch-01` and
    // the OFF assertion below fails.
    //
    // Serialize with `features/reload/001`: both mutate the process-global
    // `Features`, so their set→render windows must not interleave under plain
    // `cargo test` (see FLAG_LOCK).
    let _flag_lock = FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Same fixture as `dashboard/pane/007`: a live Pi pane with no friendly
    // name, so the card title takes the `<agent-type> · <session-id>` form —
    // the exact spot the Pi identity surfaces. The cwd basename (`workspace`)
    // and session id (`orch-01`) carry no capital `Pi`, so a `Pi ·` match pins
    // the agent-type Display specifically. `last_activity = now` keeps any
    // rendered `Last: Xs ago` at `0s ago`.
    let now = chrono::Utc::now();
    let session = SessionState {
        session_id: "orch-01".to_string(),
        agent_type: AgentType::Pi,
        cwd: Some("/home/dev/workspace".to_string()),
        status: SessionStatus::Thinking,
        active_tool: None,
        started_at: now,
        last_activity: now,
        recent_events: VecDeque::new(),
        tool_count: 0,
        last_user_prompt: Some("orchestrate the release".to_string()),
        first_prompts: vec!["orchestrate the release".to_string()],
        pane_id: Some("pi-pane-1".to_string()),
        agent_id: Some("1".to_string()),
        display_name: None,
    };
    let width: u16 = 80;
    let density = CardDensityKind::Normal;
    let wide = width >= RENDER_CARD_WIDE_LAYOUT_MIN_WIDTH;
    let height = density.rendered_height(wide);

    // Flag OFF -> the Pi first-class identity is hidden. The gate is a
    // presentation switch at the render seam: a Pi pane falls back to the
    // pre-feature unrecognized-agent baseline, but the card/pane must NOT
    // become invisible.
    features::set_for_test(Features::test_with(false));
    let off = render_card_to_buffer(&session, None, Some(1), density, 0, false, width, height);
    let off_text = buffer_to_text(&off);
    assert!(
        !off_text.contains("Pi ·"),
        "experimental flag is OFF, so a Pi pane's card must NOT show the Pi \
         first-class identity (`Pi · …`); rendered card was:\n{off_text}"
    );
    assert!(
        off_text.contains("orch-01"),
        "gating the Pi identity must NOT make the pane invisible — the card \
         (session id `orch-01`) must still render with the flag OFF; \
         rendered card was:\n{off_text}"
    );

    // Flag ON -> the Pi identity surfaces (`Pi · orch-01`), matching
    // `dashboard/pane/007`.
    features::set_for_test(Features::test_with(true));
    let on = render_card_to_buffer(&session, None, Some(1), density, 0, false, width, height);
    let on_text = buffer_to_text(&on);
    assert!(
        on_text.contains("Pi · orch-01"),
        "experimental flag is ON, so a Pi pane's card must show the Pi \
         agent-type identity (`Pi · orch-01`); rendered card was:\n{on_text}"
    );
}
