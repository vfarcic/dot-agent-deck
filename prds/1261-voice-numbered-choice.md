# PRD #1261: Offer a numbered choice when a voice command names several things

**Status**: Draft — not started. Written 2026-09-29 on branch `agent/dispatch-voice-interaction-modes`, together with [PRD #1260](1260-voice-sticky-dictation-mode.md) and [PRD #1184](1184-voice-command-chains.md).
**Priority**: Medium
**Created**: 2026-09-29
**Issue**: [#1261](https://github.com/vfarcic/dot-agent-deck/issues/1261)
**Order**: [#1260](https://github.com/vfarcic/dot-agent-deck/issues/1260) → #1261 → [#1184](https://github.com/vfarcic/dot-agent-deck/issues/1184). The panel states and their precedence are defined once, in [PRD #1260's "Voice panel states and precedence"](1260-voice-sticky-dictation-mode.md#voice-panel-states-and-precedence); this PRD adds the `AwaitingChoice` state that section names and does not restate the model.
**Depends on**: [PRD #802](done/802-desktop-voice-control.md), [PRD #1195](done/1195-voice-command-registration-and-widening.md), and PRD #1260's state model (its M2 need not have shipped; this PRD's M1 can land the state model's shared parts if it goes first).

## Problem Statement

When a spoken command names more than one thing on screen, the app refuses and says so, and the user repeats themselves more specifically. The user asked for the alternative:

> Can we have something like a popup to ask me to choose (as English or numbers 1-n) between different options when a command can be interpreted as multiple operations?

**The detection is already done.** Every reference resolver returns an ambiguous result when a spoken name matches several things — `resolve_agent_ref`, `resolve_deck_ref`, `resolve_dir_ref`, `resolve_mode_ref`, `resolve_agent_type_ref` and `resolve_orchestration_ref` in `desktop/src-tauri/src/voice/outcome.rs` — and `handle_utterance_with` turns it into `VoiceOutcome::ParamAmbiguous`, which carries the matches. `switch_deck` adds one more source, `Unmet::NamedSeveral` ("you named more than one deck", PRD #1195). Today that outcome is rendered as a sentence and the utterance is discarded. What is missing is somewhere to hold the candidates and a way to answer.

## Solution Overview

When a **required** param is ambiguous, the panel shows the candidates as a **numbered list** in the voice row and waits in `AwaitingChoice`. The user answers by **number** ("two", "the second one"), by **name** ("docs-site"), or by **clicking** an entry. The chosen candidate completes the **original** command — no second call to the Commands backend — after being checked against the list that was offered and against what is on screen now. The choice **expires**, and can be **cancelled** by voice ("cancel", "never mind") or by keyboard. A choice that leads to a stop or an orchestration close still ends at the existing D5 confirmation: choosing is disambiguation, not authorisation.

## Scope

### In Scope

- **Value ambiguity, fully**: every `ParamAmbiguous` for a **required** param, from any resolver kind, and `switch_deck`'s `NamedSeveral`.
- The `AwaitingChoice` state, its rendering, its three ways to answer, expiry, cancellation by voice and keyboard, and staleness refusal.
- Carrying candidate **values** (not only labels) from the resolvers to the panel.
- A design-page note that the chooser is for genuine ties.

### Out of Scope

- **Action ambiguity** ("close the agent": close the view, or stop the agent?) — deferred, with the reason below.
- **Optional params.** An ambiguous optional value is dropped with a note listing the candidates, today (`Unmet::dropped_note`), and the dialog it opens is where the choice is made. `open_new_agent`'s `deck` is the only optional param in the table, and its dialog's Deck field — reachable by voice through `choose_deck` since #1263 — already is that choice. Offering a chooser too would put two places to answer one question on screen.
- Answering the D5 confirmation by voice. It stays answered by hand (PRD #802 D5, `voice-first-design.md` §5).
- Any daemon or TUI↔daemon protocol change (see "Contract").

### Action ambiguity: deferred, and why

The pipeline does not detect it. The Commands backend returns **one** action (`IntentAnswer { action, params }` in `voice/resolver.rs`), so there is no outcome that says two rows fit; "close the agent" is settled before the app sees it, by vocabulary and instructions — `close` claims the phrase, `stop_agent`'s `heard_as` has no "close", and `TOOL_INSTRUCTIONS` says a view-or-stop reading means the view (`voice-first-design.md` §5, "An ambiguous reading resolves to the non-destructive action"). Detecting it would need the backend to return several candidate actions, which is the response-schema change [PRD #1184](1184-voice-command-chains.md) makes for chains. So action ambiguity is deferred to after #1184's schema lands (D1 below), and when it is revisited the rule is already written: the non-destructive reading is **option 1**, and the destructive option still ends at D5.

## Technical Approach

### The state

`AwaitingChoice` holds: the original outcome's row id and `invoke`; the param that was ambiguous; the **candidates**, each a `ResolvedParam`-shaped value (`kind`, `value`, `label`, and for a deck the `deck_identity` PRD #1195 added) in the order offered; the **declaration the utterance was judged against** (screen, `VoiceDirectories`, `VoiceNewAgent`, the New agent dialog's mount instance, endpoints); and an expiry deadline. It lives in the voice panel beside `Pending` and follows [#1260's precedence](1260-voice-sticky-dictation-mode.md#precedence): it cannot open while a D5 confirmation is open (the ambiguity renders as today's sentence instead) and cannot arise while dictating (the Commands backend is not consulted).

### Candidates need values, not labels

Today the resolvers' ambiguous arms carry **labels only** — `AgentRefMatch::Ambiguous(Vec<String>)`, `DeckRefMatch::Ambiguous`, `DirRefMatch::Ambiguous`, `ChoiceMatch::Ambiguous`, and `Unmet::Ambiguous` / `Unmet::NamedSeveral` — and `ParamAmbiguous::matches` is those labels. Labels are not enough to act on: two agents can display alike (the fixture fleet in `voice_phrase_fixtures.rs` has two agents both shown as **Atlas**), and a choice must dispatch the candidate's **value** (an agent id, a deck key, a directory path, a chip id). So each ambiguous arm carries the candidate's value beside its label, and `ParamAmbiguous` gains a `candidates` field — additive, Rust↔webview inside the desktop binary. `matches` and the sentence keep rendering the first `AMBIGUITY_NAMES_SHOWN` (3) labels as today, so a surface that does not open a chooser is unchanged.

**A cap on what is offered.** A list of more than nine candidates is not offered as a choice (single-digit ordinals are what can be said reliably and shown in one row); it renders as today's sentence, which already summarises "and N more". Nine is a starting value to revisit with use.

### Answering

An utterance while `AwaitingChoice` is answered **locally, with no Commands backend call**, by a new pure function in the voice crate (working name `voice::choice::answer`), in this order:

1. **Cancel**: a whole utterance from a closed list ("cancel", "never mind", "none", "none of them", "no") cancels the choice.
2. **Ordinal**: a whole utterance that is an ordinal within `1..=n` — "one", "1", "number two", "option two", "the second", "the second one", "the last one" — selects that entry.
3. **Name**: the utterance resolves against **the offered candidates only**, using the same resolver the kind already uses (`resolve_dir_ref` over the offered directories, and so on), and must resolve to exactly one of them.
4. Otherwise it is **not an answer** (next section).

Ordinals are whole-utterance (after `whole_utterance`'s politeness edges) so "open the second tab" is not read as "two". A **click** on an entry is the third way and skips the parse.

### The answer is checked against the OFFERED list — a distinct check from `grounded`

Picking "two" is not a reference the transcript supports in the usual sense, so the choice is validated against the **offered list**, not against the utterance. This is a different check from reference grounding (`outcome::grounded`, removed 2026-09-24 — `voice-first-design.md` §6) and from action grounding (`action_grounded`): it asks *is this answer one of the entries this app put on screen, and does it name exactly one?* An ordinal outside `1..=n`, a name matching none or several of the offered entries, or a click on an entry no longer rendered is refused. It is written down as a separate check so nobody reads it as reinstating reference grounding, and so the next person does not skip it because "grounding was removed".

**The original action is not re-grounded, and does not need to be.** It was held to the transcript by `action_grounded` when the first utterance was resolved; the answer supplies only the value. No model is asked anything, so no observed name can steer the answer.

### Staleness: reuse the existing shapes

The candidates describe a moment. If the screen moves while the choice is open, an answer is refused rather than applied to a changed list. **No new mechanism** — the answer goes through the same layers a pending dispatch already does:

- **Screen and dialog context** — the panel's `SCREEN_MOVED_ON` check and `sameNewAgentDeclaration` plus the dialog mount instance (`DIALOG_MOVED_ON`), compared between the declaration held with the choice and the current one, exactly as `resolveOne` compares them across a round trip.
- **What the target surface re-checks at dispatch** — the dispatch carries the **original** declared directories and form (`declaredDirectories`, `declaredForm` on `VoiceDispatchTarget`), so `browserMovedOn` (`NewAgentDialog.tsx`) refuses a directory choice when the browser moved since the offer, `FORM_MOVED_ON` a form choice, and `chooseDeckSelection`'s identity check (PRD #1195) a deck whose address changed.
- **The chosen value still exists.** For state Rust reads itself (agents, decks, orchestrations), the answer function re-resolves the chosen value against the current snapshot and refuses one that has gone.

A refusal says the list moved and nothing ran; the choice closes.

### Not an answer

**Decision: an utterance that is not an answer, a cancel or an ordinal closes the choice, says so, and is then resolved as an ordinary utterance.** That is how the existing pending things behave: a new utterance cancels a pending dictation send and is resolved normally (`takeUtterance`'s unconditional `cancelPendingSend`), and an utterance that is not about the New agent dialog is resolved normally while it is open. Holding the choice open and refusing everything else would make the chooser a trap — the opposite of what #1260's state model asks of every state — while one extra utterance is all a mis-parse costs. The report shows both: "Choice closed." and the new utterance's own outcome.

### Expiry and cancellation

- **Expiry**: a window (`VOICE_CHOICE_WINDOW_MS`, starting value 20 s — longer than `VOICE_UNDO_WINDOW_MS`'s 10 s because a list has to be read) shown as a countdown; on expiry the choice closes and says so. Nothing runs.
- **Voice**: the cancel list above.
- **Keyboard and pointer**: the entries and a **Cancel** button are real buttons in the voice row, carrying `VOICE_PEER_PROPS` so they stay reachable behind the agent pane's modal fence; `Escape` while focus is inside the chooser cancels it. **No window-level `Escape` is added**: the agent pane and the New agent dialog already own that key at window level (`App.tsx`: "Exactly ONE `window` `keydown` listener exists for the pane"), and a second listener would close both at once. Whether the chooser should take focus when it opens is Open Question 1.
- **Precedence**: a D5 confirmation opening (by click) cancels the choice; turning voice off cancels it.

### D5 is unchanged by this

Choosing an agent for `stop_agent`, or an orchestration for `close_orchestration`, dispatches the same `confirmStopAgent` / `confirmCloseOrchestration` the unambiguous path does, which opens the confirmation naming the target, answered by hand. The choice ends when the confirmation opens (precedence: D5 outranks `AwaitingChoice`).

### Refusals do not report success

PRD #1195 found a refused dispatch rendered beside its success sentence (`refusedRef` in `VoiceControlPanel.tsx`). A choice's dispatch goes through the same `resolveOne`-shaped path — `refusedRef` cleared before, read after — so a stale or refused answer renders only its refusal and offers no Undo.

### Which ambiguities offer a choice, and which safety refusals must not become one

- `ParamAmbiguous` from `Unmet::Ambiguous` (a spoken name matching several things) — **offers a choice**.
- `ParamAmbiguous` from `Unmet::NamedSeveral` (`switch_deck`: the transcript named several decks) — **offers a choice** among the decks named. This is safe where it was not before: the user explicitly picks one of decks they themselves named, and PRD #1195's reason for refusing — that the model's pick could not settle which deck the user meant — is exactly what an explicit answer settles.
- **`Unmet::Contrast`, `NotSaid`, `NamedOther`, `WithheldChoice`, `DeckUnavailable` and `LabelsWithheld` are refusals, not ties, and never offer a choice.** 1195's Work Log records how easily a later layer can overwrite a safety refusal (`refuse_switch_beyond_selector` rewording a `Contrast` into "choose it in the Deck selector"); the chooser keys on the `Unmet` cause, carried as #1195 carries `nothing_matched`, and not on the outcome kind alone. A contrast ("switch to build, not staging") in particular must stay a refusal: offering "1. build box 2. staging" would invite the user to pick the deck they just excluded.

### The chooser is for genuine ties

`docs/develop/voice-first-design.md` gains a note: **if the chooser appears often, the vocabularies or the resolvers are wrong and should be fixed; it is not a substitute for matching well.** The phrase fixtures are where that shows up — a fixture that used to dispatch and now lands on a choice is a regression, not a new feature.

### Contract (CLAUDE.md rules 12 and 18)

Desktop-only: the `candidates` field, the answer function and the panel state are Rust↔webview inside the desktop binary. No daemon or TUI change, no `PROTOCOL_VERSION` bump and no `.breaking.md` expected; rule 12 applies if implementation proves otherwise.

### Feature flag (CLAUDE.md rule 9)

**No new flag**, following PRD #802's decision and PRD #1195's precedent: voice control is not behind `experimental`, and the desktop binary has no flag seam.

## Testing

Tiers as in [PRD #1260's Testing section](1260-voice-sticky-dictation-mode.md#testing): Rust unit tests blocking; vitest and Playwright for the panel; phrase fixtures credentialed and local-only (a `SKIP:` is not a pass); no L2 `tests/e2e_*.rs` test reaches the desktop's voice path.

| behaviour | tier | where | extends / new |
| --- | --- | --- | --- |
| each resolver's ambiguous arm carries candidate values; `ParamAmbiguous.candidates` present and in order; `matches`/sentence unchanged | Rust unit | `voice/outcome.rs` tests (the existing `Ambiguous` assertions at each resolver) | extends |
| ordinal parse: "two", "number two", "the second one", out of range, "open the second tab" not an ordinal | Rust unit | new `voice/choice.rs` tests | new |
| name answers resolve against the offered list only, exactly one | Rust unit | `voice/choice.rs` | new |
| a chosen agent/deck/orchestration that has gone since the offer is refused | Rust unit | `voice/choice.rs` | new |
| `Contrast`, `NotSaid`, `NamedOther` never produce candidates; `NamedSeveral` does | Rust unit | `voice/outcome.rs` (beside the 1195 `switch_deck` tests) | extends |
| the list renders numbered; answer by voice, name and click each dispatches the original row once | vitest | `VoiceControlCommands.test.tsx` | extends |
| screen moved / dialog remounted / browser moved / deck identity changed → refused, nothing runs, only the refusal shows | vitest | `VoiceControlCommands.test.tsx`, `NewAgentDialog.test.tsx` | extends |
| non-answer closes the choice and resolves normally; expiry; voice and keyboard cancel | vitest | `VoiceControlPanel.test.tsx` | extends |
| a stop chosen from a list opens the D5 confirmation and closes the choice | vitest | `AgentOverviewStop.test.tsx` | extends |
| entries and Cancel reachable behind the modal pane | Playwright | `desktop/e2e/voice-control.spec.ts` | extends |
| existing `param_ambiguous` fixtures still land ambiguous (not dispatched) so the chooser is reached; no fixture that dispatched before becomes a choice | phrase fixtures | `phrase_fixtures.toml` / `tests/voice_phrase_fixtures.rs` | extends |

Answers are local, so the fixtures cover only the first utterance (reaching `param_ambiguous`); everything after it is proven by unit and vitest tests.

## Success Criteria

- "open dir docs" with `docs-site` and `docs-api` on screen shows a numbered list; "two", "docs-api" or a click opens `docs-api`, with no second Commands call.
- An answer after the browser moved, the dialog closed, or a deck's address changed is refused and nothing runs.
- A contrast refusal never becomes a list.
- A stop chosen from a list still stops only after the confirmation is clicked.
- `cargo test-fast`, rule 2's clippy, `pnpm test`, linkage-check green; `PROTOCOL_VERSION` unchanged; fixtures run locally and named in the PR.

## Milestones

- [ ] **M1 — Candidates carry values.** Resolver arms, `Unmet`, `ParamAmbiguous.candidates`, the cause-keyed eligibility; no UI change. Rust tests.
- [ ] **M2 — The chooser.** `voice::choice::answer`, the panel state, rendering, click/ordinal/name answers, staleness through the existing layers, expiry, cancel, the non-answer rule, the D5 hand-off, `refusedRef` coverage. Rust, vitest and Playwright tests.
- [ ] **M3 — Fixtures, docs, changelog.** Fixture run (local, credentialed); `docs/desktop/voice.md` (what the user sees and says — rule 21), `docs/develop/voice-first-design.md` (the offered-list check as distinct from grounding; the "genuine ties" note; the list of refusals that never become a choice), `docs/develop/desktop-gui.md`; `changelog.d/1261.feature.md`. Run `docs-screenshots-review`.

**Deferred.**

- [ ] **D1 — Action ambiguity.** After PRD #1184's response schema can carry more than one action. Non-destructive reading as option 1; a destructive option still ends at D5.

## Risks

- **The chooser papering over bad matching.** Mitigated by the design-page note and by treating a fixture that newly lands on a choice as a regression.
- **A safety refusal turned into a choice.** The #1195 class; mitigated by keying on the `Unmet` cause and by the explicit test that `Contrast`/`NotSaid`/`NamedOther` produce no candidates.
- **Stale answers.** Mitigated by reusing the three existing layers rather than inventing a fourth; the risk is a surface that has no dispatch-time re-check today, which M2 must enumerate per `invoke` a choice can reach.
- **Misheard ordinals** ("to"/"two", "for"/"four"). Whole-utterance matching keeps these from firing inside sentences; a bare "to" is accepted as "two" only if fixtures show transcribers produce it, and the choice is visible before it runs.
- **Keyboard focus.** A chooser that steals focus from an agent's terminal takes keys the user meant for the agent; one that does not take focus is hard to reach by keyboard. Open Question 1.

## Open Questions

1. **Should the chooser take focus when it opens?** Default here: no, because a choice arises from speech and the user may be typing into a terminal; the entries are reachable by Tab. Revisit with use.
2. **The expiry window** — 20 s is a starting value.
3. **The cap of nine** — a starting value.

## Work Log

### 2026-09-29 — Created

Written from issue #1261 by a dispatched unit, alongside PRDs #1260 and #1184, against the state model in #1260. Decisions recorded above — value ambiguity only, action ambiguity deferred until #1184's schema can express it, the non-answer rule matching the pending dictation send, and the list of safety refusals that must never become a choice — are this document's, open to revision with a recorded reason.
