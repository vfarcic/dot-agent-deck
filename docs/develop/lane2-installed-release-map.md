# Lane-2 measurements against the installed release

Issue #2146 records the maintainer's decision of 2026-11-08: the lane-2 measurement tests run against whichever `claude` or `codex` is installed, and skip when none is. PRD #1666 part 3 gave the doorbell one set of facts per agent, which holds for any release, so a measurement that can only replay the release it was recorded on cannot do the job the decision gave it: a newer release that breaks a fact never turns it red. The recorded captures stay in the repository as the reference.

This page maps each test as it was before that change: what it pinned, what it asserted, what it now asserts against an arbitrary installed release (a FACT the deck relies on, which must hold for every release), and what holds only for the recorded release (a hash, an exact colour, a byte-identical capture), which moves to a fast-tier check of the committed captures that needs no agent. Timings that are the deck's own waits (the echo wait, the confirm wait, the helper's alarm) are facts and stay in the live tests; measured durations were already printed rather than asserted.

## How the installed release is chosen

`tests/support/installed_agent.rs` resolves `claude` or `codex` on the same contained `PATH` the harness gives a deck pane (`common::path_with_binary_dir()`, which drops the checkout and a deck's telemetry shim directory), reads its release from `--version`, and prints `INSTALLED-RELEASE: <agent> <release> at <path>`. Each test first runs the agent's credential preflight (`check_claude_available` or `check_codex_available`); a missing CLI, missing credentials or an unreadable version prints `SKIP: [e2e] …` and returns, and `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` turns that skip into a failure (CLAUDE.md rule 5). The recorders resolve the same executable from the pane's `PATH`, hash it, read its release, record both in `result.json` (`release`, `sha256`), and fail the run if the executable changed while it was recorded; the Rust side asserts the recorder ran the release it selected. Nothing refuses a release for being the wrong one.

Removed with the pins: `DOT_AGENT_DECK_CLAUDE_MENU_<release>` (every Claude measurement), `DAD_PROBE_FRESHNESS_RELEASE` and `DAD_PROBE_FRESHNESS_BINARY` (`probe_083`), the `RELEASE` constant and `codex --version` check (Codex head echo), and the `PINS` tables in `claude_release_measure.py`, `claude_editor_measure.py`, `claude_helper_alarm_measure.py`, `claude_history_submit_measure.py` and `codex_head_echo_measure.py`. Per-release twins of one body (`_317_`, `_318_`, the unnumbered 2.1.312 ones, `codex_164`) ran the same assertions on different pinned executables; on one installed release they would run the same test twice, so each set collapses to one test with no release in its name.

## Codex head echo — `tests/e2e_codex_release_measure.rs` (`inbox/doorbell/036`)

- **Was:** `doorbell_036_live_codex_164_head_echo_{large_first,large_second,large_third,large_fourth,small_first,small_second,hooks_off_first,hooks_off_second,untrusted_first,untrusted_second,controls}`.
- **Now:** `doorbell_036_live_codex_head_echo_*`, the same eleven, on the installed Codex.
- **Pinned:** `RELEASE = "0.164.0"` (the preflight asserted `codex --version` contained it; the test asserted `result.release == RELEASE`); the recorder accepted only the native executable whose SHA-256 was 0.164.0's, and refused a running Codex process whose `/proc/<pid>/exe` hashed otherwise.
- **Facts kept (live):** every staged ring reads, through the deck's evidence state and Codex's screen profile, the empty ready composer at admission; only the placeholder or a prefix of the head, and no dialog, until the head is held exactly within the echo wait (1.5 s); only prefixes of the line until it is held exactly within the echo wait; the line held exactly just before the Enter, sent at least 150 ms after the rest; the line as a transcript user message within the confirm wait (1.0 s); Codex's own record holding exactly that prompt once and nothing else doorbell-shaped; at 50 × 200 every ring read on Clean frames. The controls session's verdicts (`checked_controls`): heads counted by the strict withdrawal recogniser, partial heads refused, every delete leaving the placeholder, burst Enters kept and refused for withdrawal, staged Enters submitting.
- **Facts kept, re-anchored:** the running Codex process is the executable the recorder resolved and hashed at start (an identity check, not a pin), and that executable is unchanged at the end; `result.release` equals the installed release.
- **Reference only (fast tier, already present):** `tests/codex_head_echo_evidence.rs` pins the 0.164.0 SHA-256 and the 0.164.0 latch on every committed capture under `tests/fixtures/codex-inbox/release-0.164.0/`. Head-echo, line-echo and transcript durations stay printed, not asserted.

## Claude start-up, composer, layout and compound keys — `tests/e2e_claude_release_measure.rs` (`inbox/doorbell/036`)

- **Was:** `doorbell_036_live_release_{startup_first_four,startup_next_three,startup_last_three,small_composer,large_composer_and_layout,compound_keys}` on 2.1.312, and the same six as `doorbell_036_live_release_317_*` and `doorbell_036_live_release_318_*`: eighteen tests.
- **Now:** `doorbell_036_live_release_{startup_first_four,startup_next_three,startup_last_three,small_composer,large_composer_and_layout,compound_keys}`, six tests on the installed Claude Code.
- **Pinned:** `DOT_AGENT_DECK_CLAUDE_MENU_<release>` or the host's `claude`, whose `--version` had to start with the pinned release; the recorder accepted only an executable in its `PINS` table, and only the one pinned for the release it reported.
- **Facts kept (live):** the start-up header latches, at the PTY reader's recorded splits, the release the recorder read from the executable; the empty ready composer; raw input mode; two hundred idle samples, twenty seconds, each empty and ready with no dialog. The composer run: fifteen lines each held exactly before the Enter, the hook payload the line alone, a transcript user echo, the echo within 1.5 s, the hook and transcript within 1 s, every blocked prompt restored within the 5 s bound, the cleared and deleted composers empty and ready with no placeholder. At 50 × 200: ten resizes and ten one-column nudges, each with the profile's full-redraw marker and a width witness at the width in force. The nineteen Q32 compound-key sequences leave exactly the recorded composers.
- **Reference only:** none moved; these assertions held unchanged across 2.1.312, 2.1.317 and 2.1.318. The executable hashes the recorder pinned move to the fast check below.

## Claude menu surfaces — `doorbell_036_live_release_menu_frames`

- **Was:** `doorbell_036_live_release_menu_frames` (2.1.310, 2.1.312 and 2.1.317 in one test) and `doorbell_036_live_release_318_menu_frames`.
- **Now:** `doorbell_036_live_release_menu_frames`, the nine size and palette sets on the installed Claude Code.
- **Pinned:** `DOT_AGENT_DECK_CLAUDE_MENU_<release>` for each release (the test panicked without it) and the recorder's `PINS`.
- **Facts kept (live):** the theme and `COLORTERM` the set asked for; the structure the profile reads (`src/doorbell_screen.rs`): the main composer's prompt in the default colour under an upper rule that is not, and the overview's upper rule in the default colour over a prompt that is not; the cleared main composer with nothing left in it and the cursor after the prompt; the overview's own composer; each surface's screen signature (`/add-dir`, the file menu entry, `! for shell mode`, `Press Ctrl-C again to exit`, `describe a task for a new session`, the quoted transcript); one blocked submission, no model turn.
- **Fact added (live):** the profile's verdict on each recorded surface, the one the fast replay checks on the recorded releases: the empty composer and the composer under the transcript quotation ready and empty-and-ready; the overview, slash menu, file menu, shell mode and exit warning neither. Before, the live test recorded these screens without asking the profile, so a release whose surfaces the profile misread passed it.
- **Reference only (new fast check):** the exact foregrounds per set (the main rule, the shell glyph and the overview prompt, e.g. `Idx(244)`, `Idx(211)`, `Idx(246)` in indexed dark), checked on every committed `claude-<release>-menus-*` capture by `doorbell_036_committed_claude_menu_captures_draw_the_recorded_palettes` in `tests/doorbell_screen_profiles.rs`. The profile reads no exact colour, only which of rule and prompt is in the default colour, and the shell glyph's colour not at all.

## Claude history recall and the line after Ctrl+U — `doorbell_036_live_release_history_*`

- **Was:** `doorbell_036_live_release_history_recall_frames` (2.1.317 and 2.1.318 in one test), `doorbell_036_live_release_317_history_submit_{plain,labelled}` and `doorbell_036_live_release_318_history_submit_{plain,labelled}`.
- **Now:** `doorbell_036_live_release_history_recall_frames` and `doorbell_036_live_release_history_submit_{plain,labelled}` on the installed Claude Code.
- **Pinned:** `DOT_AGENT_DECK_CLAUDE_MENU_<release>` (the test panicked without it) and the recorder's `PINS`.
- **Facts kept (live):** `─── History 6/6 ` over the newest seeded prompt, then `5/6`; each way out starts from a recalled prompt; Ctrl+U on a recalled prompt draws the plain composer, empty and ready; the `⏸ manual mode on` footer under a recalled prompt; the profile reads no composer, no readiness and no empty box in any History-ruled frame; two blocked submissions. The line after Ctrl+U: one hook payload, the line alone without the slot's text; the line held exactly before the Enter; no dialog after it; the cleared composer empty and ready; the six seeded entries recalled intact, newest first.
- **Reference only:** none moved. The committed captures stay replayed by `doorbell_036_claude_history_recall_frames_are_refused` and `doorbell_036_claude_history_submit_replay`.

## Claude editor probe and Ctrl+G forms — `tests/e2e_claude_editor_measure.rs` (`inbox/probe/088`, `inbox/probe/089`)

- **Was:** `probe_088_live_claude_{317,318}_{editor_export_selection_and_paste_toggles,keymap_loaded_at_start,keymap_unchanged_ten_minutes,keymap_created_and_removed,project_keymap}` and `probe_089_live_claude_{317,318}_ctrl_g_forms`: twelve tests.
- **Now:** `probe_088_live_claude_{editor_export_selection_and_paste_toggles,keymap_loaded_at_start,keymap_unchanged_ten_minutes,keymap_created_and_removed,project_keymap}` and `probe_089_live_claude_ctrl_g_forms`, six tests on the installed Claude Code.
- **Pinned:** `DOT_AGENT_DECK_CLAUDE_MENU_<release>` (skipped without it) and the recorder's `PINS`.
- **Facts kept (live):** with `VISUAL` set, Ctrl+G runs it split on spaces, hands it the draft in a file under `<TMPDIR>/claude-<uid>/` as its last argument and reimports the rewritten file, with exactly one `?2004l` before the editor and one `?2004h` after it, the same in all five presses; `EDITOR` when `VISUAL` is empty; a `vi` on the search path with neither, and `code` preferred to it. A HOME `keybindings.json` that unbinds Ctrl+G and binds Ctrl+E: honoured at the first composer frame (5/5), unchanged for ten minutes (10/10), and picked up when created and when removed. Plain BEL opens the editor in some composer state; a pasted BEL is kept as text in the next export. Nothing submitted.
- **Reference only:** none moved; the committed `editor-probe-*` summaries under `tests/fixtures/claude-inbox/release-2.1.31{7,8}/` carry the executable hash the fast check below pins.

## Claude editor helper's alarm — `tests/e2e_claude_helper_alarm.rs` (`inbox/probe/090`)

- **Was:** `probe_090_live_claude_{317,318}_{armed_helper_alarm_ends_a_stalled_exchange,armed_helper_unanswered_exchange,unarmed_helper_long_edit}`: six tests.
- **Now:** `probe_090_live_claude_{armed_helper_alarm_ends_a_stalled_exchange,armed_helper_unanswered_exchange,unarmed_helper_long_edit}` on the installed Claude Code.
- **Pinned:** `DOT_AGENT_DECK_CLAUDE_MENU_<release>` (skipped without it) and the recorder's `PINS`.
- **Facts kept (live):** the helper's own behaviour under a real Claude Code: the armed branch with a stalled exchange ends 4.5–5.5 s after the branch by its alarm with no editor run and the draft kept, SIGALRM seen in the wait status or Claude's notice; the unanswered exchange ends before 4.5 s in an orderly armed exit with its notice and no bad-editor report; the unarmed branch runs the user's editor once, outlives 8 s and ends with the editor's status, Claude showing the sentinel after each 0. Nothing submitted.
- **Reference only:** none moved.

## Probe freshness — `tests/e2e_probe_freshness_live.rs` (`inbox/probe/083`)

- **Was and now:** `probe_083_claude_idle_screen_freshness_refusal_rate`, name unchanged.
- **Pinned:** Claude Code 2.1.312 from `~/.local/share/claude/versions/2.1.312`, overridable by `DAD_PROBE_FRESHNESS_RELEASE` and `DAD_PROBE_FRESHNESS_BINARY`; skipped when absent.
- **Now runs:** the installed Claude Code, with the `e2e` build's editor-probe assumption naming the installed release (`claude@<release>`) as it named 2.1.312, so what the probe path does is unchanged.
- **Facts kept (live):** the header `Claude Code v<release>` of the release selected; in each condition at least half of the probes reached the writer and at least one was written; the resize control refuses at least one probe as `Admission(Surface)`. The refusal rate itself stays reported, not asserted.

## The executable hashes, as a reference

`doorbell_036_committed_claude_captures_name_their_recorded_executable` (fast, `tests/doorbell_screen_profiles.rs`) holds the four Claude Code hashes the recorders pinned (2.1.310, 2.1.312, 2.1.317, 2.1.318) and checks every committed capture or summary under `tests/fixtures/claude-inbox/` that names a release and an executable hash against them. The Codex 0.164.0 hash was already held the same way by `tests/codex_head_echo_evidence.rs`.

## Not changed here

`tests/e2e_codex_editor_measure.rs` (`probe_092`–`probe_094`) and `tests/e2e_codex_helper_alarm.rs` (`probe_095`) also pin Codex 0.164.0 (`DOT_AGENT_DECK_CODEX_0_164_0`, a default under `/var/tmp/dad-cxhe-codex-0.164.0/`, and the `PINS` in `tests/support/codex_editor_measure.py`). They are the Codex twins of `probe_088`–`probe_090`, outside issue #2146's list, and are left for a follow-up.
