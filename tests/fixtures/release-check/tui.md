# SC-10 TUI RED contract

This test-only slice stacks on SC-08 RED at `d65f47ef`. It uses the accepted optional `AttachResponse.release_checks` rows, hold-detail causes, agent/release fields and vocabularies from [the wire contract](wire/README.md). The revision-6 design §4.5 supplies the seven card sentences, §4.6 supplies the actions and §4.7 requires export inspection before writing. Q100b in the orchestrator's `plan-p3.md` amends §4.6: actions target releases, not display identities. This slice changes tests and fixtures, with no production implementation or claim that the assumed interfaces exist. SC-11 still owns desktop/voice implementation and parity verification.

## Input and rendering seam

The proposed entry point follows the existing host-metrics L1 seam as an unconditionally compiled, hidden public helper, compatible with integration tests in both debug and release profiles:

```rust
#[doc(hidden)]
pub fn render_release_check_key_sequence_to_buffers(
    keys: &KeybindingConfig,
    state: &AppState,
    selected_session: &str,
    capabilities: &[&str],
    snapshot: &AttachResponse,
    initial_mode: &str,
    restart_notice_labels: &[&str],
    input: &[KeyEvent],
    size: (u16, u16),
    execute: &mut dyn FnMut(serde_json::Value) -> Result<serde_json::Value, String>,
) -> Vec<ratatui::buffer::Buffer>;
```

The integration-test file has no `cfg(debug_assertions)` gate, and linkage-check must count its annotations. Library `cfg(test)` alone cannot expose a seam to an integration test. Ordinary UI construction must not accept this injected executor; the hidden public seam substitutes the external boundary while invoking production key dispatch, selection, prompt handling, response application and rendering on TestBackend. It remains compiled when debug assertions are off, like the sibling host-metrics and restart-notice seams. This is an implementation requirement, not a verified property of an entry point that is currently missing. The separate e2e outcome driver below remains feature-gated.

Return the initial frame and one frame after each key. Capabilities represent the connected daemon's Hello, independently of whether the snapshot contains records: absent or unrelated capability sets must withhold commands and controls and show upgrade guidance in agent settings. Mode is supplied separately because SC-08's snapshot has no settings field. The selected session provides the current agent/release even when there is no journal row.

An additive hidden public `render_release_check_event_sequence_to_buffers` has the same arguments and return type, with `input: &[crossterm::event::Event]` in place of `&[KeyEvent]`. It returns the initial frame and one frame per event, passing `Event::Paste` through production bracketed-paste handling, not splitting it into synthetic keys. The key helper may delegate to this event helper. Both helpers share production dispatch/rendering and the external adapter boundary; neither implements its own validator or escape routine. This missing event helper is a contract for implementation, not executed paste coverage.

### Additive selected-record diagnostics needed from SC-08

SC-08's snapshot contract currently has no per-record diagnostic. Add optional `fact` and `reason` to each release-check row, using the same bounded tolerant enums as HoldDetail, with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Missing/null diagnostics omit their keys; unknown enum strings become `other`. This additive data change needs no protocol bump or identity selector. SC-08's implementer must add corresponding serde/old-reader coverage; this slice pins consumption and display only.

Details reads the selected row's diagnostic, never the selected pane's unrelated hold. The selection fixture has an inconclusive/launch row and an unavailable/no_process_containment row while the pane says tripped/layout. Unknown fact/reason cases require understandable fallbacks. The ordinary inconclusive fixture uses launch; no_process_containment belongs to Unavailable.

## Actions and adapters (Q100b)

- Re-check sends exactly `{"op":"release-check-run","agent":"claude"}`: the current identity for that agent, even when an older row is selected.
- Cancel (`c`) asks before sending exactly `{"op":"release-check-cancel"}`: the running check, even when another completed row is selected. The shared one-word dismiss label is `confirmDismiss` (`Back`) from `inbox-display.json`, read by both clients. Back has default focus: bare Enter or Escape preserves the check and restores the selected row; Tab then Enter selects and accepts Cancel. The question and dismissal have snapshots.
- Forget (`f`) uses the same Back/default-dismiss guard before sending agent/release and removing records of that release. Tab then Enter confirms; bare Enter or Escape preserves the records. Details and the confirmation show the release-wide Forget sentence only when several identities share the selected release; a single identity still asks. Tests cover one or two identities of selected older `2.1.312` while the pane's current `2.1.313` stays passed, plus the original current-release case. Exact requests and visible results pin the release rather than the display identity.
- Export operates on the selected release, including its records for both displayed identities. Select older `2.1.312` from a current `2.1.313` pane: preview and write must name `2.1.312` exactly, and the current release stays visibly passed.
- A forgotten row offers Re-check and Close. Forget/Export keys are inert. No-reference offers Close and the §4.5 prompt advice; it does not offer an unusable Re-check. Mode off offers settings/prompt advice and no Re-check until enabled.

The sixteen-hex identity is a display abbreviation, not an authority key. No identity selector is sent. The external adapter enforces exact run/cancel/forget requests and exact preview/write export scope so an accidentally added identity field fails.

Export and mode are local adapter operations, not new daemon request variants. Preview sends `{"op":"release-check-export-preview","agent":"claude","release":"2.1.313"}`, returns `{"ok":true,"files":[{"name":"record.json","content":"…"},…]}`, and writes nothing. Fixture names/content stand in for an export manifest and scrubbed inspection material; the production exporter must supply its actual list. The user sees record.json, trace.txt and README, can select and open each in Details, return to the list, and cancel. The scope line states that the selected release's records are included. Only Continue opens the directory prompt; only its final confirmation calls `release-check-export` with agent/release/dir. Escape from either list/inspection or the directory prompt writes nothing. The preview includes the bounded-scrubber limitation in user words. These tests exercise production UI preview/prompt handlers, not real file loading, the scrubber or persistence.

Inspection treats recorded terminal output as inert text in both clients, independently of secret-pattern scrubbing. Display each C0 control other than LF, DEL, and each C1 control as a printable lowercase two-digit `\xNN` escape: ESC becomes `\x1b`, CR becomes `\x0d`, and C1 CSI becomes `\x9b`. LF delimits content rows and must not appear in a rendered buffer cell. Preserve printable text and literal backslashes; do not interpret CSI cursor/colour/mode sequences, OSC 0 titles, OSC 52 clipboard writes, or other embedded terminal commands. The preview renderer applies this presentation rule to file contents before creating UI text; it is not a claim about scrubbing or modifying exported bytes. SC-11 must apply the same rule to desktop inspection. The extended Export test includes named attack sequences and every C0/C1 control plus DEL, scans the whole buffer for control-bearing cells, and compares each hostile frame with its already escaped control frame so the dialog and surrounding rows stay unchanged.

Q101b makes the stricter rule the contract for both clients: escape Unicode categories Cc (except LF), Cf, Zl, Zp and Zs (except ASCII space), plus Default_Ignorable_Code_Point. Beyond the C0/DEL/C1 two-digit escapes, use lowercase `\u{hex}`. Examples cover bidi controls, zero-width characters, tag characters, soft hyphen, invisible operators, deprecated formats, interlinear annotations, variation selectors, Hangul fillers and non-ASCII spaces. Ordinary Unicode, ASCII space, LF and literal backslashes stay literal. Lone UTF-16 surrogates are escaped in the desktop; Rust strings cannot contain them.

The inspection bound is **16,384 Unicode code points after escaping, including the marker**, not bytes and not KiB. A long preview must show the escaped prefix, omit its trailing sentinel and include the shared marker “Preview truncated; export still contains the whole file ({bytes} bytes).” The byte count is the UTF-8 length of the untruncated inspection file supplied by the adapter. The large TestBackend view provides room for the bounded content and marker; requiring the marker keeps viewport clipping from passing as truncation. Exported contents remain whole; presentation escaping and truncation do not change them.

Adapter-supplied file names, adapter `Err` messages and the echoed export directory use the same escaping and a 16,384-code-point bound after escaping including `displayTruncated` (`Display truncated.`); million-character inputs must show the escaped prefix, marker and no trailing sentinel. Unlike multiline file inspection, these single-line labels/paths also escape LF as `\\x0a`. Raw hostile file names/errors are supplied at the external boundary, and their entire buffer must equal the already escaped control render, preserving surrounding rows; every cell in every hostile directory frame must be free of controls. The TUI editable field is visible terminal text, so it shares the escaped, bounded presentation copy; it has no exemption for raw controls. Desktop editable input values have the documented DOM exception.

Typed and bracketed-pasted destinations containing any Cc character (C0 including LF, DEL or C1) refuse Export with shared `invalidDirectory` guidance and send no write. A paste starts from an already valid directory so silently dropping the paste or only its controls cannot pass. A destination containing bidi/zero-width characters but no Cc remains allowed: preserve the raw directory in the write request and display its escaped echo before and after success. Both clients pin this acceptance as well as control rejection; presentation escaping changes neither adapter names nor the write destination.

Mode sends `release-check-mode` with mode; successful replies contain updated release_checks and mode. Choosing off during a running check opens a confirmation with the same Back label and default-dismiss guard; Escape or bare Enter keeps ask/running and issues no operation. Tab then Enter accepts off; the adapter returns a stopped inconclusive/runner_stopped record, allowing the renderer to show off and cancellation. Switching back to auto restores the setting. This synthetic reply tests UI response application, not actual cancellation, store preservation or exclusion of local admission; SC-09b must prove those daemon effects.

## Remappable keys and modal scope

Defaults remain v for Details, R for Re-check and p for agent details; M changes the mode in agent settings. The uppercase defaults are proposals; both clients' implementers must check them. These are ActionSpecs, not unconditional character handlers. Proposed dashboard config names are release_check_details, release_check_run, agent_details and release_check_mode, with context gating on the last one.

Details has its own `[release_check_details]` section: move_down=j, move_up=k, recheck=R, cancel=c, forget=f, export=e and close=Esc. Export subviews use the modal's navigation/close bindings and their own ActionSpecs for open=Enter, continue=w and confirm=Enter, active only in the appropriate subview. Register those subview names in a dedicated scope (for example `[release_check_export]`) rather than colliding with dashboard FocusPane. The remap test sets dashboard release_check_details=F2 and modal cancel=F3. Old keys become inert, hints show F3, and F3 outside Details cannot cancel. Test outcomes concern the rendered dialog and accepted external operations, not internal routing enums. The existing dashboard c/e actions must not receive modal keystrokes. Dashboard d stays Back to Pane, r Rename and m Host.

The seam seeds a real restart notice from `restart_notice_labels` (empty for ordinary cases). The remap case names SC10-BACKGROUND-NOTICE, presses old modal `c`, closes Details and requires that notice still visible. A subsequent dashboard `c` must visibly dismiss it as a positive control: accidental modal fallthrough would otherwise be indistinguishable from an inactive background binding.

M cycles auto → ask → off → auto when no check is active. With a running check, ask → off requires the explicit confirmation described above. The off settings line omits Re-check; auto and ask explain their meaning where displayed.

Off with a queued but not running check needs no confirmation, in both clients; its accepted reply stops the queued check. This is intentional: work has not started. Cancel and Forget still ask before acting in both clients, per Q101b, regardless of whether the opener was a keypress, click or speech. Confirmation words and the preview marker live in the shared JSON fixture.

## Shared words and proposed design additions

`tests/fixtures/inbox-display.json` now contains a releaseChecks table for card templates, diagnostic words, Details, modes and controls. The existing status/030 case consumes the seven templates directly as well as snapshotting the rendered notices. SC-11 must consume and check the same new words; adding the fixture is not proof of desktop parity.

Proposals for the previously unspecified words: layout → “the screen layout changed”; launch → “it couldn't be started”; no_process_containment → “the deck can't isolate the check from your machine here”. The surrounding card sentences supply the prompt/retry advice. Details likewise says what happened and gives a next action. Unknown facts/reasons have tolerant user-facing explanations.

**Design addition proposed for §4.5:** “waiting: approve the check for Claude Code 2.1.313 in Details, or send it a prompt.” This is for ask mode with no queued/running check, not an inconclusive result. It is stored under `approval_proposal` and the L1 approval case pins the initial card before pressing Re-check. It introduces no agreed wire cause here: SC-09b/SC-10 must agree how the existing hold plus mode/snapshot represents waiting for approval. The remaining Details/settings/control wording in the snapshots is proposed SC-10 vocabulary to copy into the design and SC-11, not literal revision-6 text.

Snapshots are manually authored expected output, not accepted from incorrect rendering. Details strips padding, blank rows and its rectangular frame while preserving row order, cursor, wrapping and controls. Expected text is reviewed against the design; actual rendered Details cannot be visually reviewed until the missing seam exists. Queue fixtures use running position 1, queued position 2, the last allowed position 8 and absent-position guidance rather than inventing a number.

## Stand-in outcome driver and policy barriers (L2)

`DOT_AGENT_DECK_E2E_RELEASE_CHECK_CONTROL=<isolated directory>` is a proposed driver whose definitions and call sites must be compiled only with `#[cfg(feature = "e2e")]`. A normal build ignores the variable and cannot enter that driver. The directory contains scenario.json with schema 1, the exact pane_id, profile standin, canonical agent/release, display identity, progress running and position 1. The driver validates the named pane's actual profile, not the JSON assertion: it must refuse a mismatched pane or a non-stand-in. Refusal logs to the sandbox daemon's log and writes `refused.json` containing `{"reason":"not_standin","pane_id":"…"}` for the negative control. Keep that filename and its writer inside the e2e-gated driver module, with its call sites gated too; do not introduce an ordinary-build env-controlled acknowledgement writer. That acknowledgement is test-only, not admission authority.

The negative case runs the harmless Python composer with profile=false, never an agent CLI or runner. It requires refusal and zero input through completed policy decisions before and after a synthetic pass. This unprofiled composer already cannot receive doorbell input, so refusal/no-input does not prove that no record authority was granted: a broken driver could also publish or admit a record without causing input here. An e2e build must not let this driver admit an actual production-profile generation; implementation verification must check that actual-profile validation returns before publication/admission. Compile-time exclusion from an ordinary build is an implementation requirement to inspect and verify during GREEN; these lane-1 invocations build with e2e and do not certify ordinary-build exclusion.

The positive fixture holds the outcome until result.json appears. It feeds the real queue/admission and snapshot path, never directly edits the TUI summary, injects input or acknowledges an inbox message. The test uses the existing `DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR` controlled clock and `clock_step`: clock-observed acknowledges a completed policy tick plus its spawned attempts, then the recorder drains. Only then is an empty input history meaningful. A second completed policy step with the check still held proves that focus did not type a human byte.

Before sending Enter, the test observes the TUI's COMMAND indicator (using Ctrl+D only if needed), then waits for the pane-mode hint after focusing. Enter happens before pass, with the check still held. After pass the test sends no Enter: it waits for the submitted notification, then uses the synthetic integration's normal fetch/ACK and verifies the cleared card and unchanged captured pane pid. No real agent, check runner or credential is involved.

## RED status and implementation obligations

Missing SC-08 interfaces and the L1 entry point are compile-RED, not executed behavior failures. The card case independently executes and fails on today's generic fallback. Without a driver the positive PTY case fails before delivery assertions, and the negative case fails before refusal assertions; report each reason separately. The implementation must rerun each failing test in isolation, the render modules, the PTY module and fast tier, plus fmt, full clippy, linkage-check and Scenario generation. Do not claim actual record mutation, exporter scrubbing, persistent modes, normal-build driver exclusion or desktop parity from these substitutes.
