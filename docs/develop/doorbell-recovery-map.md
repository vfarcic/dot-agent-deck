# Input-box check: how it recovers after a failed attempt

This page maps how the deck schedules the next check of an agent's input box (the composer probe) after an attempt fails, and records the slow recovery issue #2147 reported and what changed. It is for maintainers; the user-facing description of the card is in `docs/troubleshooting.md`.

## The observed sequence

Five lane-2 runs under load (PRD #1666 part 3, the act30 review; the clearest is `p318-ctrlc.log`) showed the same steps: a person's Ctrl+C clears a draft; about 165 ms later the doorbell tick asks the next check, and admission refuses it when it is scheduled with `refusal=Surface`, because the frame does not show the composer yet; the card says the deck can't check the box (`CannotCheck`); 30 s later the retry is written, the editor helper answers `Uncertain` and the card briefly shows `cannot_type: input_mode_cooked`; about 10 s after the write the probe's segment lapses unmatched, `lapse_due` counts a failure, and the next retry waits 2 min. The run ended 80 s after the lapse with no further check.

## Where each step is decided

**The tick's decision.** `tick_probe` in `src/agent_pty_segments.rs` runs on every doorbell tick (`DEFAULT_TICK`, 200 ms, `src/doorbell.rs`). With no check outstanding it asks a probe when the box is touched and `GenerationInput::probe_due` says the schedule is due; with a check outstanding it asks Claude's lapse retry when `GenerationInput::lapse_due` allows it.

**Admission when scheduled.** `AgentPtyRegistry::composer_probe` in `src/agent_pty_segments.rs` asks `admission::admit` (`src/composer_probe/admission.rs`) with the facts read at scheduling, `surface` among them (`surface_shows_composer`: a Clean frame, at least the profile's minimum width, no dialog, the ready screen, and the composer, or for Codex an empty placeholder). Admission answers the first conjunct that fails, and `Surface` is checked after every conjunct except O5, so a `Surface` refusal means everything before it passed. A refusal there was counted by `GenerationInput::probe_failed`, the same as every other refusal; since #2147 it goes through `scheduling_refused` (next section).

**The card.** `src/doorbell.rs` shows `CannotCheck` when the box is touched and `AgentPtyRegistry::composer_probe_failing` is true. That was `failures > 0`; since #2147 it is `Schedule::failing`, which is also true during a short recheck after a `Surface` refusal, so the card reads as before.

**The written check's settlement.** `Live::settle_probe` in `src/composer_probe/epoch.rs` settles a probe whose record decided: an `Armed` empty read the deck owns applies the empty read, another `Armed` read waits on the box (`Schedule::non_empty`), and anything else is a failed check (`Schedule::failed`), unless that probe's lapse was counted already.

**The lapse.** `GenerationInput::lapse_due` in `src/composer_probe/epoch.rs` finds a lone Claude probe whose segment was written more than `T_TRIGGER` (10 s, `src/composer_probe/query_ledger.rs`) ago with nothing matched and nothing held behind it. It counts that probe as one failure (`Schedule::failed`) once, and allows the lapse retry when the schedule is due again.

**The waits.** `Schedule` in `src/composer_probe/probe.rs` holds them. After a failure the next attempt waits `BACKOFF` (30 s, 2 min, 10 min), then `BACKOFF_LAST` (30 min) each; the third failure in a row marks the observer broken, which is presentation only. A non-empty read waits `NON_EMPTY_RECHECK` (30 min) or until the epoch moves. An empty read clears everything. A person's op makes a broken observer's next attempt due at once (`Schedule::person`). A writer refusal only because the screen moved since scheduling counts as a failure only every `MOVED_PER_FAILURE` (4) in a row (`Schedule::moved`).

## Which step made recovery slow

The `Surface` refusal at scheduling. It wrote nothing and only said the agent had not finished drawing after the person's key, yet it counted as a full failure: the next check waited the first backoff step (30 s), and the lapse that followed became the second failure in a row, so the retry after it waited the second step (2 min). In the observed run that is 30 s + 10 s + 120 s from the refusal to the next attempt, longer than the 80 s left.

The `Uncertain` answer and the lapse are not the slow part on their own: a written check that lapses unmatched is a real failed attempt, and counting it once with a 30 s wait is unchanged.

## What changed (#2147)

A refusal when scheduled for the screen alone now goes through `scheduling_refused` to `Schedule::surface`: the next attempt is due after `SURFACE_RECHECK` (2 s) and counts no failure, and only the `SURFACE_PER_FAILURE`th (4th) such refusal in a row counts as one failure, as `MOVED_PER_FAILURE` does at the writer. A failure or a read starts the run again. Every other refusal when scheduled counts as before, and a lapse retry's refusal still counts nothing there.

So after a transient `Surface` refusal the next check is asked about 2 s later, and a lapse after it is the first failure (a 30 s wait). A screen that never shows the composer (a dialog left open, a kept draft hiding Codex's placeholder) still walks the backoff steps, each after three extra rechecks that write nothing, and is never asked more often than every 2 s. `probe_101_*` in `src/agent_pty_segments.rs` and `src/composer_probe/probe.rs` pin both halves (`tests/CATALOG.md`, `inbox/probe/101`).

## What did not change

Whether any refusal applies: admission still decides every attempt, the screen conjunct included, at scheduling and again at the writer. Nothing is typed into a box that is not proven empty. The `Uncertain` and `cannot_type` handling, the lapse accounting, `BACKOFF`, `BACKOFF_LAST`, `BROKEN_AFTER`, `MOVED_PER_FAILURE` and the TUI↔daemon contract are unchanged: the card's `wake_hold` keeps its meaning (`cannot_check` still shows right after a `Surface` refusal), and the only status that sees the difference is the `e2e` build's `ComposerScheduleStatus`, whose `failures` now stays 0 through the short rechecks.
