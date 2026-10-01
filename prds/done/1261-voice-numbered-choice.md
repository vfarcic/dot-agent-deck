# PRD #1261: Offer a numbered choice when a voice command names several things

**Status**: Complete — M1, M2 and M3 implemented; this PRD delivers value ambiguity only. D1 (action ambiguity) is **not done** and **moved to [PRD #1184](../1184-voice-command-chains.md)** as a dependent follow-up. Written 2026-09-29 on branch `agent/dispatch-voice-interaction-modes`, together with [PRD #1260](1260-voice-sticky-dictation-mode.md) and [PRD #1184](../1184-voice-command-chains.md).
**Priority**: Medium
**Created**: 2026-09-29
**Issue**: [#1261](https://github.com/vfarcic/dot-agent-deck/issues/1261)
**Order**: [#1260](https://github.com/vfarcic/dot-agent-deck/issues/1260) → #1261 → [#1184](https://github.com/vfarcic/dot-agent-deck/issues/1184). The panel states and their precedence are defined once, in [PRD #1260's "Voice panel states and precedence"](1260-voice-sticky-dictation-mode.md#voice-panel-states-and-precedence); this PRD adds the `AwaitingChoice` state that section names and does not restate the model.
**Depends on**: [PRD #802](802-desktop-voice-control.md), [PRD #1195](1195-voice-command-registration-and-widening.md), and PRD #1260's state model (its M2 need not have shipped; this PRD's M1 can land the state model's shared parts if it goes first).

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

The pipeline does not detect it. The Commands backend returns **one** action (`IntentAnswer { action, params }` in `voice/resolver.rs`), so there is no outcome that says two rows fit; "close the agent" is settled before the app sees it, by vocabulary and instructions — `close` claims the phrase, `stop_agent`'s `heard_as` has no "close", and `TOOL_INSTRUCTIONS` says a view-or-stop reading means the view (`voice-first-design.md` §5, "An ambiguous reading resolves to the non-destructive action"). Detecting it would need the backend to return several candidate actions, which is the response-schema change [PRD #1184](../1184-voice-command-chains.md) makes for chains. So action ambiguity is deferred to after #1184's schema lands (D1 below), and when it is revisited the rule is already written: the non-destructive reading is **option 1**, and the destructive option still ends at D5.

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

**Decision: an utterance that is not an answer, a cancel or an ordinal closes the choice, says so, and is then resolved as an ordinary utterance** — from `Idle`, because the choice has already ended by then. That is how "type on" enters `Dictating` from `AwaitingChoice` without the two ever being pending together ([#1260's precedence](1260-voice-sticky-dictation-mode.md#precedence)). That is how the existing pending things behave: a new utterance cancels a pending dictation send and is resolved normally (`takeUtterance`'s unconditional `cancelPendingSend`), and an utterance that is not about the New agent dialog is resolved normally while it is open. Holding the choice open and refusing everything else would make the chooser a trap — the opposite of what #1260's state model asks of every state — while one extra utterance is all a mis-parse costs. The report shows both: "Choice closed." and the new utterance's own outcome.

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

- [x] **M1 — Candidates carry values.** Resolver arms, `Unmet`, `ParamAmbiguous.candidates`, the cause-keyed eligibility; no UI change. Rust tests.
- [x] **M2 — The chooser.** `voice::choice::answer`, the panel state, rendering, click/ordinal/name answers, staleness through the existing layers, expiry, cancel, the non-answer rule, the D5 hand-off, `refusedRef` coverage. Rust, vitest and Playwright tests.
- [x] **M3 — Fixtures, docs, changelog.** Fixture run (local, credentialed); `docs/desktop/voice.md` (what the user sees and says — rule 21), `docs/develop/voice-first-design.md` (the offered-list check as distinct from grounding; the "genuine ties" note; the list of refusals that never become a choice), `docs/develop/desktop-gui.md`; `changelog.d/1261.feature.md`. Run `docs-screenshots-review`.

- [x] **Round 3, change 2 — The choice as a dialog.** The numbered choice moves out of the voice row into a modal dialog centred over the screen, in `ConfirmDialog`'s look: what was ambiguous, the entries as a numbered list, the 20 s countdown and Cancel. It takes focus on entry 1 and gives it back on close (Open Question 1 reversed); number keys pick, Escape cancels, both on the dialog element. Drawn above the agent pane and the New agent dialog. vitest and Playwright tests; docs screenshot regenerated.

- [x] **Round 3, change 6 — Only usable daemons in the New agent dialog.** The deck field lists only daemons that can take a new agent; the others, and their long explanations and buttons, stay on the overview and the Daemons screen. Voice (`choose_deck`, `open_new_agent`) still refuses a hidden daemon by name, in one line with a short reason class. Rust, vitest and Playwright tests.

- [x] **Round 3, change 3 — Numbers on lists while voice is on.** The dashboard's agent rows (one sequence across every daemon), the Daemons screen's tiles (agreeing with the `1`–`4` keys) and the New agent dialog's daemons, directories and Mode chips (one sequence in reading order) show a number before each item while voice is on; with voice off they look as before. A bare spoken number selects the item showing it, answered locally against the declared on-screen list with no Commands call; a list that changed while the number was being said, and a number no item shows, are refused; a count that is also another item's name offers the numbered choice. Number keys select on the dashboard and the dialog's lists, never in a field. Rust, vitest and Playwright tests.

- [x] **Round 3, change 5 — Filter the directories by voice.** "filter \<text\>" sets the New agent browser's Filter box as if typed, and "clear filter" empties it (`filter_directories`, `clear_directory_filter`, both requiring the directory listing). The model extracts the text from a longer request ("show only those starting with letter D" is "d"); `voice::filter::grounded_filter_text` keeps it only if the user said it, and the report quotes what was applied. Matching stays contains, case-insensitive, for typing and voice alike. Rust, vitest and Playwright tests; the browser fixture carries both rows.

**Deferred.**

D1 below is deferred, not done: "close the agent" and every other reading the Commands backend settles into ONE action still never offers a choice, because the backend's answer cannot carry a second action until PRD #1184's schema lands.

- [ ] **D1 — Action ambiguity.** *Moved to PRD #1184 (its "Dependent follow-up: action ambiguity" section).* After PRD #1184's response schema can carry more than one action. Non-destructive reading as option 1; a destructive option still ends at D5.

## Deviations from the design above, as built

Recorded here so the sections above can be read as the design and this as what shipped.

- **Two copies of the answer rule.** `voice::choice::answer` (Rust, behind the `desktop_voice_choice` Tauri command) is the one production answers with. `answerChoiceLocally` (`desktop/src/lib/voiceChoice.ts`) answers for a runtime with no such command — the browser preview and vitest — because those runtimes have no bridge into Rust, and the chooser's panel behaviour would otherwise be untestable above Rust unit tests. It keeps the order and the closed lists but matches names against labels only, does not refuse a whole-utterance name of something unlisted, and does no liveness check. So vitest and Playwright prove the panel with the fallback, and the Rust path is covered by `voice/choice.rs`'s unit tests and by the manual walk (`desktop-gui.md` step 12c). Both copies are named in `voice-first-design.md` section 8 so a change to one is not mistaken for a change to both.
- **A refused answer closes the choice.** The design said an answer is "refused"; as built, an out-of-range ordinal, a name matching several offered entries, or an entry that has gone closes the choice with the refusal rather than keeping it open for another try. One more utterance ("say the command again") is the cost; a list that stays open after a wrong answer invites a second guess against the same, possibly stale, list.
- **A name is refused only as a whole utterance.** A name that resolves to nothing offered is refused only when the whole utterance is, word for word, the name of something on screen or of an offered entry that has gone. A sentence merely containing a name ("open the tester") is a non-answer and goes on to be resolved as a command, per the non-answer rule.
- **A tie is offered only on the row's LAST param.** Every shipped row has one param, so this changes nothing today; a tie on an earlier param would dispatch the chosen entry without the params after it, so it keeps its sentence and offers no choice.
- **The 20 s window shipped as written** (`VOICE_CHOICE_WINDOW_MS`), still a starting value (Open Question 2).
- **The chooser has its own staleness sentences.** The design reused `SCREEN_MOVED_ON` / `DIALOG_MOVED_ON`, whose "while that was being worked out" describes a round trip — wrong for a click, which makes none. The same checks now refuse with `VOICE_CHOICE_SCREEN_MOVED_ON` / `VOICE_CHOICE_DIALOG_MOVED_ON` ("…after the choice was offered…").
- **The list is never squeezed by the sentence beside it.** The docs screenshot showed the second entry cut mid-word by a long "matches more than one" sentence; the choice group no longer shrinks and is capped at 60% of the row instead, so the sentence is elided first and a long list scrolls inside its cap. *(Superseded by PR #1451 round 3, change 2: the choice is no longer in the row at all — see the Work Log.)*
- **The chooser is a dialog that takes focus (PR #1451 round 3).** The "Keyboard and pointer" design above put real buttons in the voice row and left focus alone (Open Question 1). As built after round 3, the choice is a modal dialog centred over the screen that focuses entry 1 and restores focus on close, answers number keys, and cancels on Escape; the Work Log entry for 2026-10-01 has why.

## Risks

- **The chooser papering over bad matching.** Mitigated by the design-page note and by treating a fixture that newly lands on a choice as a regression.
- **A safety refusal turned into a choice.** The #1195 class; mitigated by keying on the `Unmet` cause and by the explicit test that `Contrast`/`NotSaid`/`NamedOther` produce no candidates.
- **Stale answers.** Mitigated by reusing the three existing layers rather than inventing a fourth; the risk is a surface that has no dispatch-time re-check today, which M2 must enumerate per `invoke` a choice can reach.
- **Misheard ordinals** ("to"/"two", "for"/"four"). Whole-utterance matching keeps these from firing inside sentences; a bare "to" is accepted as "two" only if fixtures show transcribers produce it, and the choice is visible before it runs.
- **Keyboard focus.** A chooser that steals focus from an agent's terminal takes keys the user meant for the agent; one that does not take focus is hard to reach by keyboard. Open Question 1.

## Open Questions

1. ~~**Should the chooser take focus when it opens?** Default here: no, because a choice arises from speech and the user may be typing into a terminal; the entries are reachable by Tab. Revisit with use.~~ **Reversed in PR #1451 round 3 (change 2): yes.** The chooser is now a modal dialog that takes focus and gives it back when it closes; see the Work Log entry for 2026-10-01.
2. **The expiry window** — 20 s is a starting value.
3. **The cap of nine** — a starting value.

## Work Log

### 2026-09-29 — Created

Written from issue #1261 by a dispatched unit, alongside PRDs #1260 and #1184, against the state model in #1260. Decisions recorded above — value ambiguity only, action ambiguity deferred until #1184's schema can express it, the non-answer rule matching the pending dictation send, and the list of safety refusals that must never become a choice — are this document's, open to revision with a recorded reason.

### 2026-09-29 — M1 and M2 implemented

- **Candidates carry values.** Every resolver's ambiguous arm is now a list of `Candidate { value, label }` in the resolver's order, and so are `Unmet::Ambiguous` and `Unmet::NamedSeveral`. `ParamAmbiguous` gained `invoke`, `candidates`, `params` and `reports` (each candidate's report, rendered in Rust so the chooser composes no sentence); `matches` and the sentence are unchanged. The safety refusals are `ParamUnresolved` and carry no candidates.
- **Where a tie is offered.** Only on the row's LAST param (every shipped row has one), so a chosen candidate is never dispatched without params after it; a tie past `MAX_CHOICES` (9) is sent with no candidates and renders as its sentence. The cap is also enforced in the webview (`VOICE_CHOICE_MAX`). A `switch_deck` tie has its candidates addressed with the Deck selector's token and identity by `address_deck_switch`, and one with a deck the selector has no token for offers no choice.
- **The answer.** `voice::choice::answer` (cancel phrase → whole-utterance ordinal → a name resolved among the offered, still-live candidates with the kind's own resolver), reached through a new `desktop_voice_choice` Tauri command that reads the agents and decks as `desktop_voice_resolve` does. A name that resolves to nothing offered is **refused** only when the whole utterance is, word for word, the name of something on screen or of an offered entry that has gone; a sentence merely containing a name ("open the tester") is a non-answer and goes on to the endpoint. A runtime with no Rust behind it (the browser preview, vitest) answers with `answerChoiceLocally` in `desktop/src/lib/voiceChoice.ts`, the same order and closed lists matched against labels and with no liveness check — a second copy of the rule, kept deliberately small, because the tests' runtimes have no bridge to reach Rust through.
- **Refused answers close the choice.** An out-of-range ordinal or a name matching several entries closes the choice with a refusal rather than keeping it open for another try; the user says the command again.
- **The browser preview** answers "open the agent" on the Daemons screen or the dashboard with a canned tie between the `connected` state's two agents (`?fixture=1&state=connected&voice=open%20the%20agent`, then e.g. `&voice=two`), so the browser tier can drive the chooser.

### 2026-09-29 — M3: fixtures, docs, changelog

- **Fixtures.** `phrase_fixtures.toml` gained a `candidates` column, required on (and only on) a `param_ambiguous` fixture and compared as a set: `switch-deck-ambiguous-box` must offer `deck-build-box` and `deck-stale-box`, and `open-agent-ambiguous-name` the two **Atlas** agents. So a tie that stops being offerable goes red rather than passing on its kind. No dispatching fixture became a choice. The harness also gained `DOT_AGENT_DECK_VOICE_FIXTURE` (run only fixtures whose name contains it — rule 6) and prints the model's own value (`model_value`) on a red fixture.
- **Two reds met on the way, both pre-existing model variance rather than this PRD, fixed at the layer that owned them** (CLAUDE.md rule 6). `switch-deck-build-box` failed 3 of 5 alone: the model answered "switch deck to the build box" with the full label `deploy@build-box`, and `switch_target`'s rule 3 (`said`) refused it because "deploy" was not spoken. A value that resolves to exactly the one deck the transcript names alone is now accepted — it adds no deck rule 4 would not dispatch — pinned by `voice_outcome_switch_deck_accepts_the_full_label_of_the_one_deck_named`. `dictate-stop-typing-in-passing` failed 2 of 5: the model marked the whole utterance as the introducing words, so nothing was typed; `dictate_to_agent`'s description now says the prefix is never the whole sentence, with this phrasing as its example, and all ten `dictate*` fixtures then passed 12 runs of 12.
- **Docs.** `docs/desktop/voice.md` has "When a command matches several things" with a generated screenshot (`voice-choice` scenario, `cargo docs-screenshots --scenario voice-choice`), and the "decided on this machine" list names the choice's answers. `voice-first-design.md` section 8 has the numbered choice (the offered-list check as distinct from grounding, the refusals that never become a choice, the genuine-ties note, the non-answer rule, staleness, the two copies) and checklist item 11; `desktop-gui.md` has the surface note and manual walk step 12c for the Rust path.
- **Changelog.** `changelog.d/1261.feature.md`, and `1261.bugfix.md` for the two fixture fixes, which a user can observe.

### 2026-09-30 — Review findings

- **A label that is also a bare control is refused.** When an offered entry's label is, word for word, a bare ordinal or cancel phrase that the utterance also is (an agent called "two" or "cancel"), `voice::choice::answer` and `answerChoiceLocally` now return `Refused` instead of reading it either way; the panel's refusal (`voiceChoiceCollision`) says to click the entry or say "number N". "number one" is still a number. Both copies share the rule through the offered labels, and `collidingChoiceEntry` gives the panel the entry's number for any runtime.
- **The user docs list every cancel phrase** ("cancel that" was missing; "nevermind" is the same words) and describe the collision rule.

### 2026-09-30 — Review round 2

- **The changelog named the wrong number.** `changelog.d/1261.feature.md` told a user with a colliding entry to say "number one"; the right number is the colliding entry's own position, so it now says to say its number.

### 2026-09-30 — Review round 3

- **A chosen stop could open a confirmation for a same-id replacement.** Choice candidates name an agent by id, which cannot tell a replacement apart, so a `stop_agent` entry answered after the daemon replaced that agent opened D5 for the new one. The offer now snapshots the selected deck and each `agent_ref` candidate's `spawnedAtMs` (the host's `agentIncarnation` getter), and `dispatchChoice` — the one place a click and a spoken answer both pass — refuses the entry with `VOICE_CHOICE_AGENT_REPLACED` when the incarnations differ (only when both are known, `incarnationsDiffer`), or with `VOICE_CHOICE_DECK_MOVED_ON` when the selected deck changed, since the id then names an agent on another deck. `voice::choice::answer`'s liveness check stays by id: its candidates carry no spawn time and the panel check covers both routes.

### 2026-09-30 — Review round 4 — one gate for every side effect

- **A chosen stop could still reach a replacement, when the replacement happened during the resolve.** Round 3 snapshotted each candidate's incarnation when the choice was OFFERED, after the response — so an agent replaced while the first utterance was still resolving was snapshotted already replaced, and the entry opened a stop confirmation for it. Incarnations are now captured with the declaration, before the resolve (the host's `agentIncarnations` getter, every agent on the selected deck), as part of the single declared context PRD #1260's round-4 entry describes; `dispatchChoice` holds the entry to that context through the same `contextLost` gate as every other side effect, and picks its `VOICE_CHOICE_*` sentence from the gate's code.

### 2026-09-30 — Review round 5 — direct stops held to the same gate

- **A direct stop could still reach a replacement.** Round 4 applied the gate's `agent` touch only to an entry chosen from a list; a direct "stop Planner" or "close review" resolved across a same-id replacement or a selected-deck change opened the D5 confirmation for the new target. `dispatchLost` in `VoiceControlPanel.tsx` now holds every dispatch — resolved directly in `resolveOne` or chosen in `dispatchChoice` — to `answer` plus `agent` for every param of an agent-targeting kind (`agent_ref`, `orchestration_ref`), keyed by kind rather than by row, so a new row taking either param is covered without being listed. A chosen `orchestration_ref` entry, which round 4 did not gate on incarnation, is covered by the same change. Reports: "Nothing ran — the agent was replaced. Say it again." / "Nothing ran — the deck changed. Say it again." for a direct command; the `VOICE_CHOICE_*` sentences for a chosen one, as before.

### 2026-09-30 — PR #1451 review

- **A new command containing an offered name answered the choice.** With "open the agent" offering Planner, "stop Planner" selected Planner through the resolvers' loose pass and ran the open. A name answer must now be the whole utterance — every word a word of an offered entry's name, an article or the kind's noun (`choice::covers`) — and anything else is a non-answer that closes the choice and resolves normally. `answerChoiceLocally` mirrors it, which also drops its label-subset-of-answer match ("Desktop implementation extra").
- **A removed agent's entry was still dispatched on click.** The gate's incarnation check compared spawn times only when both existed; an entry whose agent was declared and is now absent is refused (`gone`, `VOICE_CHOICE_AGENT_GONE`).
- **Expiry counted timer callbacks.** The offer stores a wall-clock deadline, checked before any answer is dispatched; the countdown is derived from it.
- **A rejected dictation-mode write left the "Typed …" sentence beside its error.** The failure now clears the result (PRD #1260's surface, fixed here with the rest of the round).

### 2026-09-30 — Closed; D1 moved to PRD #1184

Everything but D1 is built and ships in the PR that closes #1261. D1 needs the multi-action response schema that PRD #1184 would introduce, and #1184's M1 measured NO-GO (not now), so D1 moved into [PRD #1184](../1184-voice-command-chains.md) as a dependent follow-up rather than staying open here.

### 2026-10-01 — PR #1451 round 3, change 2: the numbered choice becomes a dialog

- **Out of the row, into a centred dialog.** The maintainer asked for the choice as an overlay over the current screen, consistent with the app's other dialogs (`ConfirmDialog`), plus number keys. `VoiceChoiceDialog` (`VoiceControlPanel.tsx`) renders a `role="dialog"`, `aria-modal` card using `.confirm-dialog`: a heading naming what was ambiguous ("Which agent?", by the offered entries' kind), the spoken word that matched several, the entries as a numbered list of buttons, the countdown (`role="timer"`) and Cancel. The voice row goes back to reporting what was heard. Clicking the scrim cancels, as `ConfirmDialog`'s does.
- **Open Question 1 reversed: the dialog takes focus.** A number key reaches only the focused element, and the alternative — a window `keydown` listener — would also fire the agent pane's and the Daemons screen's own window listeners (`Escape` closing the pane, `1`–`4` focusing tiles). So the dialog focuses entry 1 when it opens and, in the same layout effect's cleanup, gives focus back to whatever had it, however it closes — unless the close moved focus itself (a chosen agent's pane takes it). Typing mode and a choice never coexist (a choice is offered only from `idle`, PRD #1260's precedence), so this cannot take keys from dictation. The cost the old default avoided — keys meant for a terminal going to the chooser — is bounded by the 20 s window and by focus coming back on close.
- **Keys on the element, stopped there.** A digit `1`–`9` picks that entry through `dispatchChoice`, as a click does (a digit past the list does nothing); `Escape` cancels; every key is stopped at the dialog, so `Escape` over an agent pane or the New agent dialog closes only the choice. No window listener was added.
- **Above the New agent dialog, still behind no fence.** The dialog stays a DOM child of `.voice-row` (its `VOICE_PEER_PROPS` exemption), and the row is raised over `.dialog-backdrop` while a choice is open (`.voice-row[data-choice="open"]`), since a child cannot be drawn above its parent's stacking context; Playwright checks the entries are clickable over the New agent dialog, and fails without the raise. The scrim stops at the row's top edge.
- **`useInertBackground` no longer pulls focus off a voice peer.** Its walk runs on every commit and moved focus into the fence whenever it was outside; with the choice holding focus, the New agent dialog's next render took it back (measured in WebKit). It now leaves focus on an element inside a voice peer. Its stale note that "the voice surface binds no key at all" was corrected.
- Tests: vitest `VoiceControlCommands.test.tsx` (the dialog's structure and countdown, number keys, every cancel route restoring focus, Escape over an agent pane and over the New agent dialog), Playwright `voice-control.spec.ts` (centred, number key, Escape, above the New agent dialog). The `voice-choice` docs screenshot was regenerated.

### 2026-10-01 — PR #1451 round 3, change 6: unusable daemons leave the New agent dialog

- **The dialog lists only daemons that can take a new agent.** A daemon that is not connected, has not reported, has no address, was refused as a different version, or cannot list directories is no longer shown greyed out with the overview's explanation, which took too much of the dialog (one incompatible daemon's sentence ran to three lines). The explanation and its buttons are unchanged on the overview and the Daemons screen. With none usable the field says "No daemon can take a new agent now." instead of an empty list; with none configured it still says so.
- **Hidden at render only (orchestrator decision D6).** `deckChoices` keeps every deck, so `voiceDeckStep` still declares each one to Rust and `voiceChooseDeck` still finds a hidden one — filtering upstream would have turned the refusal into "has not reported yet" (Rust's `DECK_NOT_REPORTED` for an undeclared deck) or `DECK_NOT_LISTED`.
- **Voice refuses a hidden daemon in one short line.** The declared reason is now a short class (`deckUnavailableShort`: "it is not connected", "it is older than this app", "it is newer than this app", "it is a different version from this app", "it has not reported yet", …), and both refusals read "“build box” can't take a new agent: it is older than this app." — Rust's `deck_unavailable` before dispatch, and the dialog's `deckCannotTakeAgent` for a deck that stopped being usable during the round trip. Which side is older is read from the lead of the crate's own refusal sentence (`OlderSide::who`); an unknown lead reads as "a different version". The phrase-fixture assertion for `open-new-agent-on-a-deck-that-cannot-take-one` follows the new wording.
- **The browser fixture's `error` scenario now carries a deck id**, as a live incompatible daemon does, so the New agent dialog can see it and the Playwright spec can assert the one-line empty state over it.

### 2026-10-01 — PR #1451 round 3, change 3: numbers on lists while voice is on

- **The webview declares what it numbers** (decision D3). `useNumberedList(layer, entries)` publishes each surface's items in on-screen order — the New agent dialog's `dialog` layer wins over the `screen` layer (dashboard or Daemons screen) — and `DeckShell.readNumbered` hands the panel a `VoiceNumberedListDto` whose generation moves whenever the read changes. Declared whether or not voice is on, so the list is there when voice turns on; rendered (`VoiceNumber`, "3." first in the accessible name) only while it is. A screen under an agent pane or the dialog declares nothing. Desktop-internal, additive: the daemon and its protocol are untouched.
- **A bare number never reaches the model.** `resolveOne` asks `desktop_voice_number` → `voice::numbers::answer` first, from `idle` only; it reuses `voice::choice::ordinal` (with `ordinal_word` split out so the answer can tell a count from a position). Not a number, or nothing numbered, goes to the resolver as before.
- **When the list is captured.** When the microphone opens, and again on every poll that has heard no speech yet; held once speech starts. A list that changed between then and the answer (or during the answer's round trip) is refused: "The numbers on screen changed while you were saying that, so nothing ran."
- **A collision offers the choice** rather than guessing: a number said as a count ("one", "1", "number one" — not "first" or "last") that is the trailing word of another item's label, role or CLI. Such a choice is answered by the webview's rule, because its entries can be other daemons' agents, and refused if the list moved since the offer.
- **Number keys** select only where a digit means nothing else: the dashboard and the dialog's daemon list, directory list and Mode chips; never Filter, Name or Command. The Daemons screen keeps its `1`–`4`, and tile numbers are in the same order. Daemon tiles now carry an accessible name (their role).
- **Shaped for paging (change 4):** a paged listing declares its current page only, so numbers restart at 1 per page and a page turn is a new generation; the answer rule needs no change.
- Tests: Rust `voice::numbers` (valid, not a number, out of range, stale, collision, the wire shape) and `voice_numbered_lists_are_bounded`; vitest `VoiceNumberedLists.test.tsx`, Playwright `voice-numbered-lists.spec.ts` (tester's). No phrase-fixture row: a bare number never reaches the model, and "open number three" is an ordinary command with nothing new to pin.

### 2026-10-01 — PR #1451 round 3, change 5: filtering the directories by voice

- **Two rows, both gated on the directory listing** (decision D5): `filter_directories` takes a new `filter_text` param and reports "Filtering by “\<text\>”."; `clear_directory_filter` reports "Filter cleared.". With no listing declared each is refused with its own hint ("Not here — filtering needs the New agent dialog's directory listing; …").
- **Grounded, not trusted.** The model picks the text; `voice::filter::grounded_filter_text` accepts it only when its words occur in the transcript, adjacent and in order, or when it is one letter named after "letter". Anything else is `param_unresolved` and the box is left alone.
- **One change path.** The dialog sets the box through the same `changeFilter` a keystroke takes, and refuses a browser that moved on since the utterance was judged. Typing and voice both match "contains, case-insensitive"; a Playwright test pins a typed capital `D` to the same listing as a spoken `d`.
- **The browser fixture carries both rows**, with `requires` honoured from the declared listing and a small stand-in for the model's extraction (a named letter, else the words after the opener) held to the same grounding rule.
- Tests: Rust `voice::filter` and four phrase fixtures (gpt-5-mini, 3 runs); vitest `VoiceControlCommands.test.tsx`; Playwright `voice-filter.spec.ts` (tester's).
