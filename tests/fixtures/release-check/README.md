# Release-check evaluator contract fixtures

These fixtures pin SC-01 of the revision-6 self-check design. `oracle.json` is a reviewed, synthetic 4 × 12 terminal: row zero is `ABCD` in the old frame and `ABXY` in the next frame, the other rows are blank, and the cursor is at (0, 0). `controls.json` contains the raw terminal operations inserted between those frames. No template is learned from the candidate. The shipped captures remain in `claude-inbox/r5wake/` and `codex-inbox/r5wake/`; the golden checks replay their recorded facts separately from the synthetic full-frame oracle. Those old recordings lack the future sandbox templates, termios samples and complete timing evidence, so their measurement result is not a full self-check or publication authority. SC-15 still owes the real reviewed sandbox templates.

The tests run on Linux and macOS without reading a real HOME, process identity, credential or agent executable. macOS reads the same committed bytes; these tests do not cover its cdhash, Keychain or containment.

## Assumed interface for the coder

The tests intentionally fail to import the not-yet-existing `dot_agent_deck::release_check::evaluate`. No production shim is supplied. That module exports the following measurement interface; names can change by agreement, but the controls must not be weakened to accommodate an implementation.

```rust
pub enum Outcome {
    Passed,
    Differs { fact: String, detail: String },
    Inconclusive { reason: String },
    Unavailable { reason: String },
}
pub struct Evaluation {
    pub outcome: Outcome,
    pub complete_frames: usize,
    pub stalls: usize,
    pub accepted_t1_stalls: usize,
}
pub struct Limits {
    pub transition_bytes: usize,      // default 1_048_576
    pub transition_operations: usize, // default 100_000
}
pub struct RecordedCapture {
    pub bytes: Vec<u8>,
    pub index: serde_json::Value, // the existing committed capture index
}
pub fn evaluate_recording(record: &ReleaseRecord<'_>, capture: &RecordedCapture) -> Evaluation;
pub fn evaluate_key_effects(reference: &[DialogKeys<'_>], observed: &[DialogKeys<'_>]) -> Evaluation;
pub fn evaluate_oracle(reference: &OracleReference, capture: &OracleCapture, limits: Limits) -> Evaluation;
```

`Outcome` implements `Debug + PartialEq + Eq`, `Evaluation` implements `Debug`, `Limits` implements `Default`, and `OracleReference` and `OracleCapture` implement `serde::Deserialize`. `OracleReference`'s fixture schema is rows, cols, templates (`name`, `phase`, `ansi`), transient_modes (DEC mode numbers), allowed_controls (exact raw sequences), history_lines (exact row text with trailing blanks ignored), variable_regions and fields. Each template's ANSI is interpreted from a fresh blank terminal to fix cells, attributes, modes and cursor, rather than replayed over its predecessor. An omitted mode has its initial-terminal value. The phase's allowed templates must remain disjoint. A template's identity and phase are data, not failure knobs.

`OracleCapture`'s schema is rows, cols, phase, framing (`synchronized` or `quiet_gap`), reads (`at_ns`, `bytes`), gap_events (`after_operation`, `elapsed_ns`), quiet_observations (default empty, schema below) and phase_ended. Operation positions are **counts**, so 6 means after six completed operations, counting a whole CSI as one operation and every printed character as one. The first previous frame is blank. The oracle consumes supplied operation-indexed boundary evidence; it does not derive new gaps from replay chunk timestamps. This allows timing-preserving rechunking, including boundaries inside escape sequences. A long neighbour-cell/blank stall increments both stalls and accepted_t1_stalls. Stalls do not reset either transition budget.

Q96 amends revision-6 §2.4: measured terminal silence is a second boundary source, distinct from a next-read arrival gap. Each `quiet_observations` entry carries `after_operation`, `off` (byte offset in this oracle segment), `start_ns` (time of the last actual read), `observed_ns` (the driver's monotonic time after a successful bounded quiet wait), and `elapsed_ns` (exactly `observed_ns - start_ns`). It closes a quiet-gap frame only when at least Q = 50,000,000 ns of silence was actually measured and the parser has no unfinished operation. An observation shorter than Q supplies no boundary. The driver retains the last-read provenance; replay carries these fields verbatim without re-deriving timing or interpreting replay timestamps as original clock evidence.

There is **at most one boundary per operation position**: an arrival gap of at least Q supersedes a quiet observation at the same position, even if the quiet observation was saved at an earlier checkpoint. The collector omits the superseded observation from that checkpoint's later cumulative oracle input; the evaluator also deduplicates if both are supplied, counting the frame/stall only once. Repeated quiet observations at one position likewise cannot invent extra frames. A boundary comes **never from a phase end**, deadline, EOF, timeout or kill. Without qualifying measured silence or an arrival gap, a quiet-gap phase end remains `Inconclusive { reason: "unterminated frame" }`; measured silence cannot complete a pending CSI/OSC/UTF-8 operation. Older synthetic controls may supply derived gap events directly; no assertion authorizes inventing them from a finite capture's end.

`evaluate_recording` checks the observable facts present in an existing capture's index (header, empty and typed composer, transcript reads, deletes and layout edges). It is fact-scoped replay: it neither invents absent termios/hook/timing evidence nor treats an incomplete shipped Codex record as permission to run. Matching a recorded capability whose record says `Failed` is agreement: Claude's post-delete placeholder must remain absent. `evaluate_key_effects` compares the measured tables, key by key, against the reference rather than deriving them from terminal output. The full driver will combine these measurements with oracle and runtime evidence before a check can pass for publication.

The doctored recordings use `tests/support/release_check_recording.rs` to rebase named points, read boundaries and resize offsets together. Replay applies each geometry edge before consuming the bytes following its offset, retaining the recorded order of edges that share an offset; a screenshot at the offset precedes those edges. The existing `doorbell_036_release_recogniser_negatives` exercises the same helper without the missing evaluator: it retains all 30 Codex edges, checks the rebased point/read indexes, replays the resize/nudge screenshots unchanged, and checks that the shifted initial header no longer latches while the original does. This proves fixture consistency and the header control's intended difference; the future evaluator must still execute the named `release_recogniser` outcome assertion.

The asserted inconclusive reasons are `unterminated frame` and `limit: transition`. Missing or ambiguous complete templates name `unrecognised_frame` or `ambiguous_frame`; an unsupported per-operation cell names `transient_content`; an undeclared mode, control or discarded history line names `mode_transient`, `unrecognised_control` or `unrecognised_history`. Each Differs retains nonempty detail. The other environment/runner reasons and retries in design §2.5 belong to later slices; this fixture does not claim to exercise them.

## SC-00 safety and cleanup map

The RED changes the existing `doorbell_053_inheritance_rules`, retaining the exact incomplete-record control and adding exact Go 0.164.0 beside unmeasured 0.165.0, distance-four and distance-five minors, patches, a pre-release, another major, older releases and no records. Once SC-00 is implemented, keep this test and consider renaming it to describe exact-record admission. The Q35-11 editor-probe tests stay: `doorbell_053_an_editor_record_decides_no_ring`, `doorbell_053_editor_records_move_no_verdict_in_any_table`, `doorbell_053_the_editor_probe_assumption_moves_no_shipped_verdict`, and the separate `editor_probe_permission.rs` tests. The enumeration's inherited-only existence assertion must be removed with the variant; its no-ring-permission assertions remain.

In `e2e_inbox_codex_doorbell.rs`, `codex_027_unrecognised_screen_and_missing_submit_evidence_stay_safe` replaces the old `codex_027_what_is_not_a_recognised_dialog_never_sets_the_tripwire`: the same unrecognised-screen/no-byte, swallowed-Enter/queued and `Update now`/successful-delivery assertions now run on the measured release, with the retired tripwire assertions removed. `codex_027_a_dialog_on_the_head_gets_nothing_more` now also explicitly checks no first-stage byte before the measured settle. Existing exact-release tests keep all three dialog refusals at admission, zero bytes to those dialogs, delivery after they close, exactly eight first-stage bytes when a dialog appears on the head, no rest/Enter/delete after it, the queued task and visible `dialog_during_ring` hold, and the first write's token head and settle bound.

Delete `doorbell_053_tripwire_is_per_release`, the now inheritance-only `doorbell_053_timing_set`, `codex_027_a_dialog_on_an_inherited_release_sets_its_tripwire` and `codex_027_a_dialog_mid_attempt_on_an_inherited_release_sets_its_tripwire` when removing the production variants. The mixed-table exact-record checks formerly in `doorbell_053_timing_set` now live in the kept `doorbell_053_key_effects_and_own_settle`: beside slow 0.160.0 (16-second first modal) and fast 0.164.0 (2-second first modal), the exact releases independently retain 24-second and 3-second settles. Other surviving safety assertions live there and in `codex_027_a_measured_codex_is_rung_in_two_stages_after_its_settle_window`, `codex_027_a_dialog_at_admission_gets_no_byte` and `codex_027_a_dialog_on_the_head_gets_nothing_more`. Keep the exact-record portion of `doorbell_053_a_staged_line_needs_its_head_echo_measured`, replacing its final inheritance assertions with unmeasured-release assertions or deleting only that obsolete tail. In `doorbell_053_exclusion_never_reaches_codex`, change the retired inheritance clauses to unmeasured release and keep its exclusion-isolation controls. Keep the other exclusion enumerations. `doorbell_053_inheritance_is_codex_only` may be renamed, but its Claude/other-agent unmeasured controls stay.

The deleted-clause search found `release_tripwire` and `inherit_source_incomplete` in activation and generation/logging paths, and none as a declared variant or literal in `daemon_protocol.rs` or committed fixture data. `doorbell_refusal` feeds a typed-refused presence into the hub's WAKE choice; this does not establish that deleting inheritance has no observable effect. The production SC-00 author still owns the rule-12 semantic-contract decision. This test-only work changes no protocol or daemon code.

## Control evidence map

Every row names a test in `tests/release_check_evaluate.rs`. Until the evaluator exists these are interface-RED, not verified behavioral failures; GREEN must later execute each assertion.

| Design control | Test function | One-line evidence |
| --- | --- | --- |
| Golden captures | `doorbell_036_evaluator_shipped_captures_match_their_records` | Four shipped releases, both Claude colour environments, match their recorded facts. |
| R2 header one column over | `doorbell_036_evaluator_header_one_column_over` | Insert cursor-right before Codex's actual header, retaining and rebasing all 30 geometry edges; `release_recogniser`. Shared-helper consistency and scanner difference also execute in `doorbell_036_release_recogniser_negatives`. |
| R2 RGB (56,56,56) | `doorbell_047_evaluator_rgb_56_transcript` | Change every actual RGB-55 user background to RGB-56; `transcript_echo`. |
| R2 missing echo | `doorbell_036_evaluator_missing_echo` | Remove the first typed trial's drawing bytes; `echo`. |
| R2 placeholder after delete | `doorbell_036_evaluator_placeholder_after_delete` | Substitute the original placeholder screen for the first deletion; `placeholder_empty`. |
| R2 committing key | `doorbell_036_evaluator_committing_key` | Make inert `d` commit in update; `key_effects` detail names update and d. |
| R3 overlay drawn and erased in one read | `doorbell_060_overlay_within_one_read` | Row 2 briefly contains `!`, blank in both neighbours; `transient_content`. |
| R3 split at overlay and inside CSI | `doorbell_060_overlay_rechunked_inside_escape` | Whole, 1-byte, 7-byte and explicit after-`!` splits retain the failure. |
| R3 neighbour glyph moved | `doorbell_060_neighbour_glyph_at_another_position` | `A` written in blank row 2 fails despite belonging to both frame alphabets. |
| R3 T-1 partial erase | `doorbell_060_t1_partial_erase_residual` | Erase CD at their own cells before XY; accepted residual. |
| R3 clear then redraw | `doorbell_060_clear_then_redraw` | ED and redraw are accepted. |
| R3 coloured erase | `doorbell_060_erase_under_coloured_background` | Indexed-237 blank erase cells are accepted against matching neighbour backgrounds. |
| R3 two sync blocks in one read | `doorbell_060_two_sync_blocks_in_one_read` | Count two good frames; a bad first frame cannot hide behind the second. |
| R3 ending mid-transition | `doorbell_060_phase_ends_mid_transition` | A started but unfinished sync frame is `unterminated frame`. |
| R3 restored mode | `doorbell_060_undeclared_mode_restored` | Mouse mode 1000 on/off still yields `mode_transient`. |
| R3 OSC 52 | `doorbell_060_osc52_control` | Clipboard sequence yields `unrecognised_control`. |
| R3 undeclared history | `doorbell_060_undeclared_history_line` | Scroll future ABXY off before its frame; cells are allowed but history is not. |
| Scroll-adjustment positive control | `doorbell_060_scroll_adjusts_previous_frame` | Old ABCD shifts from row 1 to row 0 before WXYZ replaces it; shifted old cells pass. |
| R3 counted stall | `doorbell_060_quiet_gap_stall_counted` | A partial erase at a 50 ms gap passes with one stall. |
| R3 transition bound | `doorbell_060_transition_limits` | Byte and operation budgets independently produce `limit: transition`. |
| R4 long T-1 stall | `doorbell_060_long_t1_stall_reported` | A 500 ms stall passes, counted and explicitly marked accepted T-1. |
| R4 foreign cell past Q | `doorbell_060_non_neighbour_past_quiet_gap` | Pause with `!` visible for 500 ms; still `transient_content`. |
| R4 stalls share one bound | `doorbell_060_stalls_share_transition_bound` | A generous-budget control passes and counts three stalls. Three 13-byte/3-operation stalls and the final redraw independently exceed a 26-byte budget and a 12-operation budget; resetting either counter at a stall would incorrectly pass. |
| R4 preserved rechunking and changed gap | `doorbell_060_timing_preserving_rechunk_and_changed_gap_control` | Original gaps pass at every split; a new erased-frame boundary makes restored CD fail. |
| Q96 measured silence and deduplication | `doorbell_060_measured_terminal_quiet_closes_once` | Measured terminal quiet closes one frame at every split; absent/short silence and unfinished CSI remain unterminated. A later arrival at the same operation gives one boundary. |
| R5 timestamp-merging trap | `doorbell_060_timestamp_merge_must_not_invent_gap` | 100/140/180 ms reads have 40 ms gaps; merging under 100 would invent 80 ms, which the actual helper never derives. |
| R2 old composer/new overlay | `doorbell_060_old_composer_new_overlay_region` | Completed old composer plus Confirm? is `unrecognised_frame`. |
| R2 old transcript/new modal | `doorbell_060_old_transcript_new_modal_region` | Submitted template with an added modal is `unrecognised_frame`. |
| R2 extra dialog | `doorbell_060_extra_dialog` | A completed unknown dialog is `unrecognised_frame`. |
| R2 ambiguous_frame | `doorbell_060_ambiguous_frame` | Duplicate matching templates yield `ambiguous_frame`. |
