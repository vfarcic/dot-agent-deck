# The deck's line plus Enter after Ctrl+U on a recalled prompt (Q143b)

Measured on 2026-11-04 for PRD #1666 part 3's Claude activation, answering revt318's finding U2-1. After a person recalls a prompt with Up and clears it with Ctrl+U, Claude Code draws the plain empty composer, but it keeps its history position (the next Up recalls the next-older entry). The production profile reads that drawing as empty and ready, so the doorbell may type its line there and press Enter. This folder records what that does, on real Claude Code 2.1.317 and 2.1.318.

**Re-running (issue #2146).** The four tests named below are now two, `doorbell_036_live_release_history_submit_{plain,labelled}`, which measure the Claude Code installed on the harness's `PATH` (40 trials a run) instead of a pinned executable; the recorder no longer refuses an unpinned one, and the executable hashes below are checked on these captures by `doorbell_036_committed_claude_captures_name_their_recorded_executable` (`tests/doorbell_screen_profiles.rs`). [`docs/develop/lane2-installed-release-map.md`](../../../../docs/develop/lane2-installed-release-map.md) maps each test.

**Outcome: benign, 80 of 80 trials.** In every trial the hook received one submission whose prompt was the deck-shaped line alone. The recalled entry's text never leaked into it, no dialog appeared, and afterwards the history held all six seeded entries intact and in order, the cleared slot's text included. That holds from the plain-drawn state and from the cleared slot under its History label alike.

## How it was measured

- **Tests:** `doorbell_036_live_release_317_history_submit_{plain,labelled}` and `doorbell_036_live_release_318_history_submit_{plain,labelled}` in `tests/e2e_claude_release_measure.rs` (lane 2, run locally on 2026-11-04 with `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1`, serially; all four passed).
- **Executables:** the pinned copies `/var/tmp/dad-m317-claude-2.1.317/installed/claude` (SHA-256 `eb224419…`) and `/var/tmp/dad-t318-claude-2.1.318/installed/claude` (`708c77b2…`), each checked against the release recorder's pins before and after every captured child.
- **Isolation:** each trial ran in the TUI harness's disposable HOME, never `~/.claude`, with real interactive Haiku. The release recorder's private `UserPromptSubmit` hook blocked every submission, so no model turn was made.
- **Recorder:** `tests/support/claude_history_submit_measure.py` (SHA-256 `1ae3a103…`, in each `.json` as `recorder_sha256`). It imports the committed release recorder `tests/support/claude_release_measure.py` (`79931911…`, `release_recorder_sha256`) unchanged, for its pins, its hook and its `Recorder`.
- **Each trial:**
  1. Seed the same six earlier-session prompts as the release recorder's `history` kind (`HISTORYONEalpha` … `HISTORYSIXfoxtrot`).
  2. From the fresh composer, press Up t times to recall slot k = 7 − t. Trials 1 to 5 recall `6/6` down to `2/6`.
  3. Send Ctrl+U once (`x1`) or four times in one write (`x4`).
  4. **plain:** ring straight away, from the plain rule over the empty box. **labelled:** first press Up (which recalled the next-older entry, `History k−1/6`, 40/40) and then Down back onto the cleared slot, `History k/6` over the empty box, and ring there.
  5. The ring copies the deck's: `(token) dot-agent-deck: message <id> waiting, run: dot-agent-deck inbox fetch` built with the production `doorbell_line`, written in one write (Claude's records are not staged). The recorder waits for the line on screen, then 200 ms, then sends a lone CR.
  6. Wait for the hook, then for the blocked line to come back as a draft. Clear the draft with deletes, press Up eight times and Down nine times, and read the disposable HOME's `.claude/history.jsonl`.
- **Counts:** 2 releases × 2 cases × 2 press counts × 2 sizes (50×200, 24×80) × 5 trials = **80 trials**. All 80 rang; every labelled trial reached its cleared `History k/6` slot.

## Files

`<release>/<case>-x<presses>-<rows>x<cols>-t<trial>.bin` holds the raw PTY bytes. `.json` holds the recorder's index (`read_lens`, `points`, `edges`), which the fast tests' `fixture()` loader reads, plus:

- `outcome`: what the test read at each step, with the profile's verdicts;
- `hook_prompts` and `hook_events`: the hook payloads' prompts and event names;
- the trace, executable and recorder SHA-256s;
- `load`: the 1-minute load average and CPU PSI line at the recorder's start and end.

`tests/support/claude_history_submit_export.py <test-log> <this-folder>` writes these files from the captures the `--no-capture` test log names, and `--check` confirms them byte for byte. `doorbell_036_claude_history_submit_replay` (fast tier) replays all 80 through the production profile.

## What the trials show (both releases, both sizes, both press counts)

- **The cleared state is drawn with the placeholder, not a blank box: 80/80.** The plain rule sits over the dim `Try "…"` placeholder (random per launch), with `Ctrl+Y to paste deleted text` on the row above the rule. The profile reads it as empty and ready, 80/80. This differs from the committed `r5wake/claude-*-history-*` captures, whose `after-ctrl-u` and `slot-cleared` points draw a blank box. Those captures had submitted a blocked line earlier in the session and these did not; whether that is the reason was not measured. The press count is not the reason: one and four presses drew the same.
- **The labelled slot is `History k/6` over the same placeholder, 40/40, and the profile reads no composer there** (not empty and ready), as it does for every History-ruled frame. So the deck would ring in the plain-drawn case and not in the labelled one. The labelled trials ring anyway, to measure what admitting that frame would mean.
- **The first character typed redraws the rule plain.** At the echo, and 200 ms later at `typed`, no trial drew a History label over the typed line: 80/80. The profile read the line as held exactly (`holds` = `Exact`, the ring's echo check), 80/80.
- **Enter submits the line alone.** Each trial got one `UserPromptSubmit`, prompt equal to the line byte for byte, with no slot-k text: 80/80. The submit hook fired 144–295 ms after the CR, and the echo appeared within 78 ms of the write. With the hook blocking it, Claude put the line back as an editable draft under the plain rule (80/80), and no dialog was drawn.
- **The history afterwards is the six seeded entries, intact and in order.** The first Up after clearing the blocked draft recalled `History 6/6` `HISTORYSIXfoxtrot` (80/80), so after a submission the walk starts from the newest entry, not from slot k. Up then reached `1/6` and stayed there, and Down came back to the plain composer. Slot k's text was recalled intact under its label in every trial. The disposable HOME's `history.jsonl` was unchanged (80/80).

**Not measured.** With every submission blocked, the line never entered the history, so whether an accepted doorbell line is added to it, and what a person's next Up recalls after a real model turn, is not shown here. Nor are timings other than the recorder's echo wait plus 200 ms, a Ctrl+U on a wrapped recalled prompt, a slot recalled from a session that had already submitted, or any release other than 2.1.317 and 2.1.318.

## For the profile

The plain-drawn after-Ctrl+U state is already admitted and is benign in these 80 trials, so nothing there needs refusing. The cleared slot under a History label (blank in the committed `r5wake` captures, the placeholder in these) is the same state with a different label, and typing or Enter there behaved the same in the 40 labelled trials. These trials therefore give no reason to keep refusing it beyond the profile's general rule for History-ruled frames. Whether to admit it is the coder unit's ruling, not made here. If it is admitted, the profile would have to tell the blank (or placeholder) slot under a History label apart from a recalled prompt under one, since only the first is empty.

## Load context

The 1-minute load average at the recorders' starts and ends ranged from 1.37 to 20.53 on 16 CPUs, and CPU PSI `some avg10` peaked at 26.84 %, from other sessions' work on the same machine. Every trial's `load` field holds its own readings. The harness printed no load-context verdict for the passing tests. The one it printed during development, on a run stopped for a harness race (fixed before this run), read QUIET.
