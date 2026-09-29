# PRD #1184: Chain several voice commands from one utterance

**Status**: Draft — not started, and the one of the three that may be deferred: M1 is a go/no-go measurement, and the milestones are written so each stops cleanly. Written 2026-09-29 on branch `agent/dispatch-voice-interaction-modes`, together with [PRD #1260](1260-voice-sticky-dictation-mode.md) and [PRD #1261](1261-voice-numbered-choice.md).
**Priority**: Low
**Created**: 2026-09-29
**Issue**: [#1184](https://github.com/vfarcic/dot-agent-deck/issues/1184)
**Order**: [#1260](https://github.com/vfarcic/dot-agent-deck/issues/1260) → [#1261](https://github.com/vfarcic/dot-agent-deck/issues/1261) → #1184. A chain pauses by entering the states those two define; the model is in [PRD #1260's "Voice panel states and precedence"](1260-voice-sticky-dictation-mode.md#voice-panel-states-and-precedence) and is not restated here.
**Depends on**: [PRD #802](done/802-desktop-voice-control.md) (which excluded chaining from v1), [PRD #1223](done/1223-desktop-new-agent-dialog.md) (action grounding, whole-utterance rows, declaration staleness), [PRD #1195](done/1195-voice-command-registration-and-widening.md), PRD #1261 (for a chain that pauses on an ambiguous step), PRD #1260 (for "type on" as a last step).

## Problem Statement

One utterance produces at most one action. "Open the overview and then the tester" resolves to one command or to a no-match; it never performs two.

PRD #802 excluded this on purpose: *"Performing that chain automatically is deliberately out of v1 — a single utterance silently performing two operations is where a misfire gets expensive."* That still holds, and PRD #1223 since added guards a chain must satisfy **per step**, not once per utterance (the issue's comment): action grounding, reference resolution against what is on screen, D5 confirmation, and declaration staleness. The comment also records a case that is **not** a chain: *"Select code and dot-agent-deck directories"* is one step with two mutually exclusive values, because the New agent flow chooses one working directory.

## Solution Overview

The Commands backend may answer with an **ordered list of up to three steps**, each carrying the **span of the transcript** it came from. The app accepts a chain only when:

- every step is **grounded in its own span** of the transcript, spans in order and not overlapping;
- every step is **valid against the state the previous steps would produce** — step N's `callable` is computed against the predicted screen after steps < N;
- the utterance carries **no contrast word** (the #1195 lesson: "open settings, not the overview" must not become two steps);
- every step's position is allowed (a consequential step, a dictation step and "type on" only **last**).

If any step fails validation, **nothing runs**. Execution then runs the steps in order and **stops at the first refusal**, saying which step refused and which ran. A step that is ambiguous pauses the chain in `AwaitingChoice` (#1261), holding the rest. A consequential last step ends at the D5 confirmation.

## Scope

### In Scope

- The multi-step answer, per-step grounding and validation, the position rules, execution with per-step revalidation, the stop-and-report rule, the pause in `AwaitingChoice`, chain Undo as defined below, and phrase fixtures including one that must refuse mid-chain.
- The mutually exclusive case, answered explicitly (below).

### Out of Scope

- Chains longer than three steps.
- Any step whose effect the table cannot predict, except as the last step (below).
- `start_new_agent`, `submit_prompt` and `discard_new_agent` in any chain (below).
- Action ambiguity ([PRD #1261 D1](1261-voice-numbered-choice.md)), though the schema change here is what makes it possible later.
- Any daemon or TUI↔daemon protocol change.

## Technical Approach

### The answer carries steps, each with its span

Today `IntentAnswer` is `{ action, params }` (`voice/resolver.rs`) and every response schema — the generated one in `voice/schema.rs`, and the two request builders in `voice/openai.rs` and `voice/remote.rs` — is pinned by value in tests. The answer gains an optional `steps` list; each step is `{ action, params, said }`, where `said` is **the words of the transcript this step came from, copied from it**. A single-action answer keeps today's shape, so every existing fixture and backend path is unchanged.

**`said` is verified exactly as dictation's prefix is.** `voice::dictation::strip_opening` already proves a model-marked prefix is genuinely the front of *our* transcript, token-wise, and fails closed. Each step's `said` must be a contiguous token run of the transcript, the runs must be in order and must not overlap, and the text between runs must be connective words from a closed list ("and", "then", "and then", "after that", "next", plus punctuation). A `said` that is not a run of the transcript refuses the whole chain. **The model marks spans; it never supplies the words a step is judged on** — the same property dictation holds for typed text.

### Per-step action grounding

Each step's row is grounded by `action_grounded` against **its own `said` run, not the whole transcript**. Against the whole transcript, "open docs and start it" would ground both `open_dir` and `start_new_agent` on either half, so a hostile label could smuggle a second action into a sentence that supports only the first — the issue comment's point. `heard_as_whole` rows (`submit_prompt`, `discard_new_agent`) can never be a step: the whole utterance cannot *be* their phrase if it is a chain, and that is the right answer, since whole-utterance evidence exists precisely because they are irreversible.

### No contrast word anywhere in a chain

PRD #1195's Work Log is the evidence that exclusion language defeats per-part matching: "switch to build, not staging", "rather than", "away from". A chain is refused outright if the transcript contains any `CONTRAST_MARKERS` word (`voice/outcome.rs`, the closed list #1195 built for `switch_deck`) outside a single step's span — "or" and "instead" included, so "open the overview or the deck" is never two steps. The refusal says "say one command at a time". This is deliberately conservative, for #1195's reason: a false refusal costs one utterance; a wrong pair costs a user who cannot tell which half went wrong. **Per-step reference checks run on the step's span too**: `switch_target`'s transcript-side naming (`decks_named`, `contrast_marker`) is handed the step's `said`, not the whole transcript, so a deck named in another step is not counted as named for this one.

### Validation against the predicted state

`CommandRow::callable(screen, directories, new_agent)` answers for one declared state. A chain needs step N judged against the state steps < N would produce, which the table does not describe today. So the table gains a column, **`lands_on`**: the screen a row's dispatch leaves the user on, or `stays` for a row that changes no screen. Only rows with a `lands_on` value can be **non-last** steps; any row without one can only be **last**. Working assignments, to be verified against each registry entry's `run` in M2:

| row | `lands_on` | can be non-last |
| --- | --- | --- |
| `open_overview` | `overview` | yes |
| `open_deck` | `deck` | yes (gated by the flag as today) |
| `open_agent` | `agent` | yes |
| `open_settings` | `stays` (an overlay; see below) | no — `close` would then mean Settings |
| `close` | — (what closes depends on what is on top) | no |
| `switch_deck` | `stays` | yes — but the next step resolves against the new deck's agents, which are not loaded until it connects, so an `agent_ref` or `orchestration_ref` step after it is refused |
| `open_new_agent`, `open_dir`, `go_to_parent`, `use_this_directory`, `choose_deck` | — (the listing and the form arrive asynchronously from the daemon) | no |
| `choose_mode`, `choose_agent_type`, `name_new_agent` | `stays` (the form stays live) | yes |
| `voice_off`, `list_commands` | — | no |
| `stop_agent`, `close_orchestration` | — (consequential, D5) | no, last only |
| `dictate_to_agent`, `dictation_on` (#1260) | — (its text or mode runs to the end of the utterance) | no, last only |
| `start_new_agent`, `submit_prompt`, `discard_new_agent` | — | never in a chain |

Validation runs in Rust, in `handle_utterance_with`, before anything is dispatched: each step's action grounding, its `callable` against the predicted screen and requirements, and its params resolved against the state that step will see. **If any step fails, nothing runs**, and the outcome names the step and its reason.

### Why `start_new_agent` is never in a chain

D5's start half was revisited on the grounds that "the confirmation re-stated what was already on screen" (`voice-first-design.md` §5). In a chain such as "make it a dispatcher and start it", the form changes and starts in the same breath, before the user has seen the change — the premise that removed the confirmation does not hold. Rather than reinstating a confirmation for one path, a start stays a separate utterance.

### The mutually exclusive case

Two steps of the **same** single-valued row ("select code and dot-agent-deck directories" → `use_this_directory`/`open_dir` twice, "make it a dispatcher and a reviewer" → `choose_mode` twice) are not a chain. **Decision: offer the values as a choice** (#1261's `AwaitingChoice`, "Which one? 1. code 2. dot-agent-deck"), when both resolve on screen; refuse when either does not. Refusing outright was the alternative and was not taken: the user named two real things and one question settles it, which is exactly what the chooser is for. Taking the first was rejected as a guess.

### Execution: in order, revalidated per step, stopping at the first refusal

The panel runs step 1 through `onDispatch` exactly as a single command runs today, waits for the host to commit it (the view change is a React state update — the next step's declaration must be read after it lands), **re-declares** the current state and checks it equals the predicted state for step 2, then dispatches step 2, and so on. This is the issue comment's "per-step revalidation", in the shape the panel already uses across a round trip (`SCREEN_MOVED_ON`, `sameNewAgentDeclaration`, and each surface's own dispatch-time check such as `browserMovedOn`). It is not a new mechanism; it is the existing one applied between steps as well as around the round trip.

**A refusal at execution time stops the chain and says exactly what happened** — "Step 2 (open the tester) was refused: \<reason\>. Step 1 (open the overview) ran." — never a silent partial chain. A step refused through `reportRefused` is caught by the `refusedRef` pattern PRD #1195 added, so a refused step can never render beside a success sentence (the #1195 `switch_deck` finding, which applies per step here).

### Pausing: ambiguity and D5

- **An ambiguous step** (a required param with several candidates) pauses the chain in `AwaitingChoice` with the remaining steps held in that state. Answering resumes the chain from that step, revalidating it and every later step against the state as it is then; cancelling, expiry or a non-answer drops the rest, and the report says which steps ran and which were dropped. A chain may pause at most once.
- **A consequential last step** (`stop_agent`, `close_orchestration`) dispatches the confirmation opener as it does alone, and the chain ends there. The earlier steps having run while the stop waits is exactly the "partial chain" risk the issue names; it is bounded because earlier steps are restricted to rows with a predictable, undoable effect (`lands_on`), and the report names them.

### Undo

**Voice has an undo today, and it is narrow:** the voice row offers **Undo** for ten seconds (`VOICE_UNDO_WINDOW_MS`) when a command moved the view, and it restores the view captured before that command (`App.tsx`'s `dispatchVoice`: the undo is offered "only where one moved", because an Undo that reversed nothing would be "an affordance lying about what it reverses"). Overlays, form edits and typed text have no undo.

So a chain's Undo **reverses the whole chain or is not offered**: it restores the view captured before step 1, and it is offered only when **every step that ran** returned an undo (was a view move). A chain containing any step with no undo — a form edit, `open_settings`, `switch_deck`, a dictation — offers none, and the report lists what ran so the user knows what to reverse by hand. Undoing only the last step was rejected: it would leave the user between two states they never asked to be in.

### Grounding in the transcript's own words, and the other #1195 lessons, per step

The 1195 review rounds on `switch_deck` recur here, one per step:

- **Grounding in the transcript's own words** — spans verified token-wise; each step grounded in its span only.
- **Stale settings** — `switchDeck` already reads `latestSettings.current`; each step reads the host's latest state at its own dispatch, never state captured at utterance start.
- **Refused actions still reporting success** — `refusedRef` per step; a refused step replaces the chain's success report.
- **Contrast words** — refuse the chain.
- **Caps refusing everything** — the three-step cap applies only to an answer that has more than three steps, which is refused with its own sentence ("say up to three things at once"). It never touches a one-step answer, so a single command can never be refused by it — the class of #1195's 256-row cap, which refused every voice command for a user over the limit.
- **Rewrites overwriting safety refusals** — a step's own refusal cause (`Unmet::Contrast`, `NotSaid`, …) is reported as-is, never re-worded into a chain-level sentence that loses it; the chain report quotes the step's sentence.

### The backend and the prompt

The schema and `TOOL_INSTRUCTIONS` gain the `steps` shape, with the rule that `said` is copied from the transcript and that alternatives or exclusions are one step or a `none`. Every pinned action-enum and schema test changes (`schema.rs`, `prompt.rs`, `openai.rs`, `remote.rs` — `voice-first-design.md` §3 lists them), and linkage rule 14's schema checks follow. **PRD #1273** (Jev as an optional Commands backend, blocked on access) models "which action?" as a single choice; a chain is not expressible in that shape, so with a backend that cannot return steps, chains are simply unavailable and single commands behave as today. That PRD should note it when it resumes.

### Contract (CLAUDE.md rules 12 and 18)

Desktop-only: the answer schema is between the desktop and the user's Commands endpoint, and the rest is Rust↔webview inside the desktop binary. No daemon or TUI change, no `PROTOCOL_VERSION` bump, no `.breaking.md` expected.

### Feature flag (CLAUDE.md rule 9)

**No new flag**, following PRD #802's decision and PRD #1195's precedent. (If M1's measurement is marginal, shipping behind `experimental` would need the flag seam the desktop binary does not have — that would be a decision for the user, not this document.)

## Testing

Tiers as in [PRD #1260's Testing section](1260-voice-sticky-dictation-mode.md#testing).

| behaviour | tier | where | extends / new |
| --- | --- | --- | --- |
| `said` verified as contiguous in-order non-overlapping runs; a `said` nobody said refuses the chain | Rust unit | `voice/outcome.rs` (beside `strip_opening`'s tests in `voice/dictation.rs`) | new |
| each step grounded in its own span only: "open docs and start it" does not ground a second action from the first half | Rust unit | `voice/outcome.rs` | new |
| a contrast word anywhere refuses; "or" is never two steps | Rust unit | `voice/outcome.rs` | new |
| predicted-state validation: `open_overview` then an overview-only row is callable; a row after an unpredictable step is refused; nothing dispatched when any step fails | Rust unit | `voice/outcome.rs`, `voice/table.rs` (`lands_on` parsing and the closed set) | new |
| position rules: D5, dictation and `dictation_on` last only; `start_new_agent`/`submit_prompt`/`discard_new_agent` never | Rust unit | `voice/outcome.rs` | new |
| same single-valued row twice → candidates for a choice | Rust unit | `voice/outcome.rs` | new |
| single-action answers unchanged; pinned schema/enum sets updated | Rust unit | `schema.rs`, `prompt.rs`, `openai.rs`, `remote.rs` | extends |
| execution in order with commit-then-revalidate; stop at first refusal with the "step N refused, steps < N ran" report; `refusedRef` per step | vitest | `VoiceControlCommands.test.tsx` | extends |
| pause in `AwaitingChoice` holding the rest; resume revalidates; cancel drops the rest and reports | vitest | `VoiceControlCommands.test.tsx` | extends |
| chain Undo offered only when every step moved the view, and restores the pre-chain view | vitest | `VoiceControlCommands.test.tsx` (and `App` dispatch tests) | extends |
| phrasings: "open the overview and then the tester" (dispatch 2 steps); "open the tester and type run the tests" (last-step dictation); **a chain that must refuse mid-chain** — "open the tester and open dir docs" on the overview (step 2 needs the New agent dialog, so validation refuses and nothing runs) and one that is refused at execution by a stale surface (unit/vitest, since a fixture cannot move the screen); "select code and dot-agent-deck directories" (a choice); "open the overview or the deck" (refused); a hostile-listing chain | phrase fixtures | `phrase_fixtures.toml` / `tests/voice_phrase_fixtures.rs` (a `steps` expectation column) | extends |

Credentialed fixtures run locally only; a `SKIP:` is not a pass. No L2 `tests/e2e_*.rs` test reaches the desktop's voice path.

## Success Criteria

- M1 records a go/no-go with numbers, and the decision is followed.
- If built: "open the overview and then the tester" performs both, reports both, and one Undo restores the starting view.
- No chain ever runs a step whose own span does not ground it; a contrast word never yields two steps.
- A chain that fails validation runs nothing; one refused at execution says which step refused and which ran.
- A consequential last step still stops at the confirmation.
- Gates green; `PROTOCOL_VERSION` unchanged; fixtures run locally and named.

## Milestones

Each milestone ends at a state that can ship or stop on its own.

- [ ] **M1 — Go/no-go measurement (no product change).** Build the `steps` schema and prompt on a branch; run a chain fixture set against the default backend repeatedly; measure span fidelity (does `said` copy the transcript?), segmentation accuracy, contrast handling, and whether **single-command fixtures regress**. **Clean stop:** if single commands regress or spans are unreliable, record the numbers here and close the issue as not now, with nothing shipped.
- [ ] **M2 — Validation only.** Schema, `said` verification, per-step grounding, contrast refusal, `lands_on`, predicted-state validation, position rules. A valid chain is still refused with "chains are not enabled yet" — so this milestone ships nothing user-visible and **can stop here** with the validation layer tested.
- [ ] **M3 — Navigation-only chains.** Execution, per-step revalidation, stop-and-report, chain Undo — for steps with a `lands_on` screen and a last step that is any navigation-safe row. **Clean stop:** a useful, bounded feature ("open the overview and then the tester") even if M4 never lands.
- [ ] **M4 — Pauses and last-step specials.** The `AwaitingChoice` pause (needs #1261 shipped), the mutually exclusive case as a choice, D5 last steps, dictation and `dictation_on` last steps (needs #1260 shipped).
- [ ] **M5 — Fixtures, docs, changelog.** `docs/desktop/voice.md` (rule 21: what to say, what is shown when a step is refused), `docs/develop/voice-first-design.md` (per-step grounding, the position rules and why), `docs/develop/desktop-gui.md`; `changelog.d/1184.feature.md` at the first milestone a user can observe (M3). Run `docs-screenshots-review`.

## Risks

- **Single commands regress** because the prompt now describes a second shape. M1 measures it before anything else is built.
- **A steered second step.** Per-span grounding and span verification are the defence; the residual is a step whose span genuinely contains one of its `heard_as` words in passing (the "evidence, not proof" residual of `voice-first-design.md` §6), bounded because non-last steps are view moves an Undo reverses.
- **Which half went wrong.** The report names every step and its outcome; this is the risk #802 excluded chains for, and the report is the mitigation.
- **Asynchronous effects** (listings, a deck connecting) make "predicted state" false for some rows; those rows are last-only by construction, and M2 must verify each `lands_on` against its registry entry rather than trusting the table above.

## Open Questions

1. **Is three the right cap?** A starting value.
2. **Should `open_settings` be allowed mid-chain** if `close` is excluded after it? Default no.
3. **Should a chain pause more than once?** Default no — one pause, so a chain cannot turn into a dialogue.

## Work Log

### 2026-09-29 — Created

Written from issue #1184 and its comment by a dispatched unit, alongside PRDs #1260 and #1261. The design — spans verified like dictation's prefix, per-span grounding, a closed `lands_on` column deciding which rows can be non-last, `start_new_agent` excluded, the mutually exclusive case offered as a choice, and Undo that reverses the whole chain or is not offered — is this document's; M1 decides whether any of it is built.
