# SC-10 TUI RED contract

This test-only slice stacks on SC-08 RED at `d65f47ef`. It uses the accepted optional `AttachResponse.release_checks` rows, hold-detail causes, agent/release fields and vocabularies from [the wire contract](wire/README.md). It changes no production file. The on-machine self-check design, revision 6, §4.5 supplies the seven card sentences; §4.6 supplies the actions, and §7.3's SC-10 supplies Details snapshots and the synthetic attached-TUI scenario. SC-11 owns the desktop/voice/shared-fixture work. No new daemon capability or production authority is proposed here.

## Minimal assumed input/rendering seam

The new surface has no existing public L1 entry point. `ui::render_release_check_key_sequence_to_buffers` is a proposed test seam, following `render_host_metrics_key_sequence_to_buffers` and `inbox_attention_run`. Its signature is:

```rust
pub fn render_release_check_key_sequence_to_buffers(
    keys: &KeybindingConfig,
    state: &AppState,
    selected_session: &str,
    snapshot: &AttachResponse,
    initial_mode: &str,
    input: &[KeyEvent],
    size: (u16, u16),
    execute: &mut dyn FnMut(serde_json::Value) -> Result<serde_json::Value, String>,
) -> Vec<ratatui::buffer::Buffer>;
```

It must run the production key handler, selection, prompt handling, response application and renderer against a `TestBackend`. Return the initial frame followed by a frame after each key. The callback substitutes the external command boundary, so no real store, daemon, exporter or agent is needed by L1; it must not substitute a reducer or renderer. The selected session supplies the hold's agent/release even if the journal list is empty. Mode is injected separately because SC-08 defines no settings field in its snapshot. Implementation may rename or reshape this seam with the tests, but replacing it with an independent rendering implementation would defeat the test.

The callback's JSON describes the external operation: the existing SC-08 wire forms for run/cancel/forget; local `release-check-export` with `agent`, `release`, `dir`; and local `release-check-mode` with `mode`. Export and mode are **test adapter operations, not proposed daemon request variants**. They stand in for the production local operations used by those controls. Successful fake replies contain `ok`, the new `release_checks` rows, and adapter-only `mode`/`exported_to` outputs. The real UI must apply the answer from its production response handling; the test does not feed a final frame or precomputed UI action back to it. Error replies leave the held row visible and show the error. These tests establish visible handling of accepted/refused operations, not durable store behavior or capability enforcement by this substitute.

## Proposed keys and words

The design explicitly leaves key choice to implementation. Proposed dashboard keys are `v` for self-check Details, `R` for Re-check on a held card and `p` for the selected agent's detail view. In that view `v`/`R` have the same meanings and `M` cycles auto → ask → off → auto on its settings line. In Details: `j`/`k` select a record, `R` re-checks, `c` cancels a queued/running check, `f` forgets, `e` opens the export-directory prompt and Escape closes. In the export prompt Enter submits and Escape dismisses. Dashboard `d` is already the plain command-mode alias for Back to Pane, `r` means Rename, `m` means host information, `s` means Schedules, `i` means message decisions and `c` dismisses the restart notice; the proposed modal keys preserve those bindings. These keys are proposals requiring the SC-10/SC-11 implementers to check against both clients' active bindings, not a claim of completed parity.

The design leaves the English for the three reason/fact insertions open. This slice proposes `layout` → “the screen layout changed”, `launch` → “the check couldn't start” and `no_process_containment` → “the check can't be cleaned up safely”. The rest of each card sentence is literal §4.5 text. The Details/control answer wording in the snapshots is likewise a proposed user vocabulary; it exposes no journal epochs, publication authority, raw reason codes or promises of check duration. The selected record's sixteen-hex identity is a display abbreviation only.

The snapshots are manually authored expected output, reviewed as text against the design, never accepted from today's incorrect rendering. The card snapshot records the actual notice rows at 180 columns; the dashboard's existing snapshots cover its surrounding layout. Details snapshots remove outer padding, blank rows and box-drawing borders, retaining the actual row order, wrapping, selected cursor, explanations and buttons. They require a bordered Details surface and fit at 120×32. Once the seam exists, run these tests and review the **rendered** snapshots by eye before claiming GREEN; authoring an expected snapshot is not that validation.

## Selected identities: an unfilled SC-08 interface

Details can list and select two identities of one agent/release. SC-08's `run(agent)`, global `cancel()` and `forget(agent, release)` cannot name an individual identity/publication. This slice does not silently add authority-selection fields or claim that its display abbreviation can select a record. It proves browsing those rows, and mutation of a selected **distinct release** through the existing verbs. Identity-specific Forget/Export/Cancel needs an agreed full-key/action-selection contract before that part of §4.6 can be claimed complete. A capability-gated future extension must not treat this display string as authority. The callback-only Export operation here is scoped to a distinct agent/release; export manifests/file inspection in §4.7 remain outside this slice.

## Stand-in check-outcome driver for L2

`DOT_AGENT_DECK_E2E_RELEASE_CHECK_CONTROL=<isolated directory>` is a proposed `feature = "e2e"`-only fixture. The directory contains `scenario.json`: `schema: 1`, the exact `pane_id`, `profile: "standin"`, agent/release/display identity, `progress: "running"` and a queue position. It applies only to the existing synthetic composer profile on that named sandbox pane. It must not start an agent CLI or a self-check runner, read a credential, or make a production-profile generation locally admissible. A normal build ignores the variable. The L2 test uses the harness's own HOME, endpoints, config and process handles.

The fixture holds the upstream result while `result.json` is absent. It reports the running hold and check through the real daemon snapshot path; the TUI remains unmodified. Atomically renaming `result.next` to `result.json` with `{"state":"passed"}` releases the stand-in outcome into the real queue/admission path for that stand-in, so the existing token-led notification must reach the existing pane. The fixture must not synthesize terminal input, change an inbox message's state or clear a TUI notice directly. The test then performs the synthetic integration's normal fetch after the visible submission; it is explicit about that being synthetic, not evidence of a real agent fetching a message. A captured pid check forbids a replacement pane from satisfying the test.

This is an assumed interface, not working driver support in SC-08. On the current branch the visible checking assertion is expected to fail because the fixture/state integration and rendering are missing. Until the driver is implemented and its hold/admission path checked, that L2 result is setup-RED and the later delivery assertions remain unexecuted. The independent card test's runtime snapshot RED does not depend on this seam. SC-09b/SC-10 must implement the fixture and rerun the whole L2 module before treating the scenario as verification.

## Coverage and remaining work

`status_030_card_names_each_release_check_hold` extends the existing card-hold catalog entry and can execute before SC-08 support. `release_check_004_*` is a new Details surface not covered by the old message-decision picker. `release_check_005_checking_card_then_delivery_without_respawn` covers its synthetic PTY transition. Every annotated function has a Scenario comment. Required checks are fmt, full four-flag clippy, fast tier, linkage-check and the scoped render/PTY targets. Missing SC-08 client symbols and the new TUI seam are compile-RED obligations, not passing gates; report them distinctly from executed failures.
