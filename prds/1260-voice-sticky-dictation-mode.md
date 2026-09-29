# PRD #1260: A sticky dictation mode for desktop voice ("type on" / "type off")

**Status**: In progress — M2 and M3 done on branch `agent/dispatch-voice-interaction-modes` (2026-09-29); M1's measurement was not taken and the overlapped capture cycle it would decide was deferred (Work Log). Written 2026-09-29 on branch `agent/dispatch-voice-interaction-modes`, together with [PRD #1261](1261-voice-numbered-choice.md) and [PRD #1184](1184-voice-command-chains.md), because the three share one voice panel state model (below).
**Priority**: Medium
**Created**: 2026-09-29
**Issue**: [#1260](https://github.com/vfarcic/dot-agent-deck/issues/1260)
**Order**: #1260 → [#1261](https://github.com/vfarcic/dot-agent-deck/issues/1261) → [#1184](https://github.com/vfarcic/dot-agent-deck/issues/1184). This PRD comes first because it defines the panel states the other two slot into; #1261 adds `AwaitingChoice`; #1184 pauses in the states the first two define.
**Depends on**: [PRD #802](done/802-desktop-voice-control.md) (desktop voice control and per-utterance dictation, shipped), [PRD #1195](done/1195-voice-command-registration-and-widening.md) (the registration guard and the grounding lessons recorded in its Work Log, shipped).

## Problem Statement

Typing into an agent by voice needs an opener on every utterance today: "type run the tests", then "type and check the logs", then "type then open a PR". The user asked for a mode instead:

> Right now, when in voice mode inside one of the agents, I need to say something like "Type ..." whenever I want it to type something in agent's prompt. Can we have something like "Type on" and "Type off" as instructions so that I say "Type on" (or something similar) and, from there on, everything I say is typed into the agent until I say "Type off" (or something similar)?

The per-utterance opener is most tiring exactly when somebody is composing a real, multi-sentence prompt for an agent.

**This asks for something that already shipped once and was removed, and this PRD has to say why a second attempt is different.** PRD #802's first dictation was a mode — "type to the tester" aimed the microphone and every later utterance was typed until an exit phrase — and it was rebuilt into today's one-utterance shape after the product owner used it (`docs/develop/desktop-gui.md`, "Dictation, which stopped being a mode"; `voice/dictation.rs`'s module comment). D6 named its two failures: **a missed exit types the exit phrase into the agent**, and **a false positive truncates somebody mid-sentence**. The rebuild removed both by removing the mode. This PRD reintroduces a mode, so both failures come back unless the design answers them — and the answer (Technical Approach, "Why the old mode's two failures do not return") is what distinguishes this from reverting the rebuild.

## Solution Overview

A mode the voice panel holds, entered and left by whole-utterance phrases and by visible controls:

- **"type on"** (and close variants), said on its own with an agent's pane open, enters **Dictating** for that pane's agent.
- While Dictating, an utterance is **not sent to the Commands backend at all**. It is typed verbatim into that agent's prompt. Nothing is sent to the agent until the user asks.
- A **small reserved set of whole utterances** stays live: the off switch ("type off"), a submit phrase ("send it"), and "voice off". The panel shows the set while the mode is on.
- **Non-voice exits always exist**: a visible **Stop typing** control in the voice row, the Voice button, and closing the pane.
- The mode **ends on any change of context** — the pane closes, its agent exits or stops accepting input, the user navigates away, the deck changes. It never follows the user to another screen.
- It is **visibly indicated** for as long as it is on.

## Voice panel states and precedence

This section is the one state model for #1260, #1261 and #1184. The other two PRDs reference it rather than restating it; a change to it is a change to all three.

### The states

| state | what it means | introduced by | how the next utterance is treated |
| --- | --- | --- | --- |
| `Idle` | one-shot commands — today's behaviour | #802 | resolved through the Commands backend as today |
| `Dictating` | every utterance is typed into one agent's prompt | this PRD | matched locally against the reserved set; otherwise typed; **never** sent to the Commands backend |
| `AwaitingChoice` | a numbered choice is on offer | [#1261](1261-voice-numbered-choice.md) | matched locally as an answer (ordinal, offered name) or a cancel; see #1261 for a non-answer |
| D5 confirmation | a stop or orchestration close is waiting to be answered by hand | #802 D5, #1223 | as today: the confirmation is answered by clicking; a second spoken stop is refused (`CONFIRMATION_ALREADY_OPEN`) |
| the existing pending answers | the New agent dialog's form and directory browser, and a pending dictation send (`Pending` + countdown in `VoiceControlPanel.tsx`) | #802, #1223 | as today |

**A chain in progress ([#1184](1184-voice-command-chains.md)) is not a state of its own.** A chain that has to stop for the user stops by entering `AwaitingChoice` or by ending at the D5 confirmation, with any remaining steps held by that state. That keeps the number of things the panel can be waiting for fixed at the rows above.

**Where each state lives.** `Dictating` and `AwaitingChoice` are held by the voice panel (`VoiceControlPanel.tsx`), in a ref mirrored into state for the reason `Pending` already is (the poll reads it between awaits). The D5 confirmation is held where it is today, by the overview (`AgentOverview.tsx`'s `confirm`); the panel does not own it and learns of it only through what the host declares. The Rust side stays stateless per utterance — `handle_utterance` keeps no memory between utterances (`docs/develop/voice-first-design.md` §2) — so whatever a state needs Rust to know travels in the declaration made with each utterance, the way the New agent dialog's state already does.

### Precedence

**D5 confirmation > `AwaitingChoice` > `Dictating` > `Idle`**, and only one of the first three at a time.

- **Entering a higher state ends a lower one**, and the report says so: a D5 confirmation opening (by click or by voice) cancels an open choice; a choice cannot open while dictating, because the Commands backend is not consulted and nothing can come back ambiguous.
- **A lower state cannot start while a higher one is pending.** "type on" while a choice is open is answered by #1261's non-answer rule, not by entering the mode; while a D5 confirmation is open, an ambiguity renders as today's sentence rather than opening a chooser.
- **Ending a state returns to `Idle`, never to a state that was pre-empted.** Dictation cancelled by a confirmation does not silently resume when the confirmation is answered; the user says "type on" again. A mode that comes back by itself is a mode the user did not see start.
- **The existing pending answers are orthogonal and unchanged.** The New agent dialog is a surface the resolver reads through its declaration, not a panel state; a pending dictation send is a countdown `Idle` already carries. `Dictating` does not arm that countdown (below), and entering `Dictating` cancels a countdown already running rather than sending it.

## Scope

### In Scope

- The `Dictating` state in the voice panel, its indicator, its reserved set and its exits, as above.
- Entering by voice ("type on" and close variants) as a whole utterance, on the agent screen only.
- Leaving by voice ("type off" and close variants, "voice off"), by the **Stop typing** control, by the Voice button, and on any context change.
- Submitting while dictating by a whole-utterance submit phrase (reusing `submit_prompt`'s whole-utterance evidence) and by the user's own Enter.
- The capture-cycle changes dictation needs so words are not silently lost between utterances (Technical Approach, "Words said while the last utterance is transcribed").
- User docs in `docs/desktop/voice.md`, design detail in `docs/develop/desktop-gui.md` and `docs/develop/voice-first-design.md`, a changelog fragment.

### Out of Scope

- The numbered choice ([#1261](1261-voice-numbered-choice.md)) and chains ([#1184](1184-voice-command-chains.md)), except where this PRD's state model constrains them.
- Changing one-shot dictation. "type run the tests" in `Idle` keeps working exactly as today, countdown included.
- A keyboard shortcut of its own for the mode (Technical Approach, "Non-voice exits", says why).
- Any daemon or TUI↔daemon protocol change. Everything here is inside the desktop binary, between its Rust side and its webview (see "Contract").
- The TUI, which has no voice control.

## Technical Approach

### Entering the mode

- **New rows `dictation_on` and `dictation_off`** in `commands.toml`, both `screens = ["agent"]`, both with **`heard_as_whole`** (the whole transcript must be one of the entries, `outcome::whole_utterance`), not `heard_as`. Entering changes how every later utterance is treated, so it must not ground on a word said in passing — "type on the tester's prompt that the build is on" is dictation, not a mode switch. Working phrase sets, to be fixed by fixtures: on — "type on", "typing on", "start typing", "dictation on", "start dictation", "keep typing"; off — "type off", "typing off", "stop typing", "dictation off", "stop dictation", "done typing".
- **A local fast path answers both before any backend call**, beside today's `local_intercept` in `outcome.rs`, as whole-utterance equality in `voice/dictation.rs` (a new `DICTATION_ON_PHRASES` / `DICTATION_OFF_PHRASES`, matched with `whole_utterance_is`). It must run **before** the `DICTATION_OPENERS` check, because "type on" today opens with the opener "type" and would type the word "on". **The cost is stated rather than hidden:** after this change a bare "type on" or "type off" can no longer dictate the single word "on" or "off" in `Idle`; "type the word on" still can.
- **Linkage-check rule 14 extends to the new lists** (`xtask/linkage-check/src/voice_command_registry.rs`): every phrase in each list appears in its row's `description`, and the lists are pairwise disjoint with `SUBMIT_PHRASES` and with each other, for the reason the existing opener/submit pair is — the order they are checked in must never be load-bearing.
- **The registry entry** (`VOICE_ACTIONS.startDictation` / `stopDictation`, `voice: true`) is served by the panel through `VoicePanelChannel`, the seam `voice_off` already uses, and targets the pane on screen — the composite `{deckId, agentId}` the existing `Pending` uses, never a bare agent id.
- **A pane that cannot take input refuses entry.** The residual `desktop-gui.md` records for one-shot dictation — nothing checks before dispatching that the pane accepts input (`terminalInput.ts`: a read lease, no live target, a finished agent) — is not acceptable for a mode, because the failure would repeat on every utterance. Entering checks writability first, from what the host already holds about the pane's agent, and refuses with the reason.

### While dictating

- **The panel does not call the Commands backend.** It still calls Speech (transcription is how it hears anything). The Rust side is asked a local question instead: the declaration carries the dictation target, and `handle_utterance_with` answers it first — after the empty-transcript check, before `local_intercept` — with no `IntentRequest` built. So no observed names and no transcript leave the machine for the Commands stage while the mode is on; `docs/desktop/voice.md`'s "What is sent where" gains that sentence.
- **Classification, in this order, each a whole-utterance equality after normalisation** (`dictation::normalise`, plus `WHOLE_UTTERANCE_POLITENESS` at the edges as `whole_utterance` already allows): (1) "voice off" phrases, (2) the off phrases, (3) `SUBMIT_PHRASES` and `submit_prompt`'s `heard_as_whole` entries, (4) otherwise, the whole transcript is typed. The order is #802's decided one — the bigger stop first — and with disjoint lists it decides nothing today; it exists so a future overlap cannot leave a live microphone after a user asked for it to stop.
- **Typed verbatim, through the existing path.** The whole transcript goes through `dictationText` (the control- and format-character scrub, the trailing space) and `sendTerminalInput`, exactly as one-shot dictation's slice does. There is no opener to strip and the model marks nothing, so `strip_opening` is not involved; "type fix the bug" said while dictating types all four words.
- **Utterances accumulate; nothing is sent per utterance.** No countdown is armed while dictating: the countdown exists because a one-shot dictation's user may not say anything else, and in a mode they are, by definition, going to. Sending is a submit phrase said on its own, or the user's Enter. After a send the mode **stays on**, so the next prompt can be composed the same way; the report says "Sent — still typing to the tester."
- **Why the submit phrase stays live, and why that is the right trade.** The issue sets out the dilemma: if "send it" is live it cannot be dictated alone, and if it is not there is no voice way to submit. `submit_prompt` already makes that trade for the whole product — `SUBMIT_PHRASES` are "the words a user cannot dictate alone" — so the mode inherits it rather than inventing a second rule. Dictating the literal words is done by saying them inside a longer utterance ("and then send it to the reviewer"), which is typed, because only the **whole** utterance is compared. The alternative designs were weighed: an accumulate-with-no-voice-submit mode strands a voice-only user with a prompt they cannot send; a per-utterance countdown in the mode sends half-composed prompts during a thinking pause, which is the failure the countdown's own doc comment warns about at five seconds.

### Leaving the mode

- **Voice:** an off phrase ends it; "voice off" ends it and turns voice off. Neither sends anything.
- **Non-voice exits.** A **Stop typing** button in the voice row, shown only while dictating, carrying `VOICE_PEER_PROPS` so it stays clickable and tabbable behind the agent pane's modal fence exactly as the Voice button is (`useInertBackground`'s exemption, currently one marker; this is a deliberate second, and `desktop/src/AgentPaneDeckIdentity.test.tsx`'s reachability equality is widened to name it). The Voice button ends it too. **No new keyboard shortcut is bound, by decision:** on the agent screen every key belongs to the agent's terminal, and the app's one window-level key there is `Escape`, which closes the pane (`App.tsx`: "Exactly ONE `window` `keydown` listener exists for the pane"). So the keyboard exits are `Escape` — which closes the pane and ends the mode as a context change — and Tab to **Stop typing**, which keeps the pane open.
- **Context change ends it**, and the report says why ("Stopped typing: the tester's pane closed."). The triggers: the view leaving that agent's pane (any navigation, `closeAgent`, `paneAgentRetired`), the pane's agent exiting or becoming unwritable, the selected deck changing, voice turning off, and the panel unmounting. Each is a place the host already observes; the mode subscribes to them rather than polling for them. **Never follows the user**: opening another agent's pane does not move the mode to it.
- **A D5 confirmation opening ends it** (precedence). One cannot be opened by voice while dictating — the stop rows are `screens = ["overview"]` and the Commands backend is not consulted — but one can be opened by a click on another surface.
- **Nothing is sent on exit.** Whatever is typed stays in the prompt, visible and editable. That is #802 D6's "never auto-submits on exit", kept.

### Why the old mode's two failures do not return

- **A missed exit** types "type off" into the prompt — visibly, and unsent, because the mode arms no countdown. The user sees it arrive and presses **Stop typing**. That is the same recovery #802 relied on ("the words are in an input the user is looking at"), minus the countdown that made it urgent.
- **A false positive cannot truncate mid-sentence**, because an exit is only ever a whole utterance. An utterance is bounded by the capture's own silence detection (`SILENCE_HOLD`, 800 ms), so "we should stop typing the logs to the file" is one utterance and is typed, and only an utterance that *is* "stop typing" stops.
- **What is genuinely new is the entry, which the old mode did not have in this form**: it was aimed by "type to the tester", a param row. This one has no target param — it targets the pane on screen, which is the rebuild's own targeting rule — so it cannot aim at an agent the user is not looking at.

### Words said while the last utterance is transcribed

**This is the risk most specific to a mode, and it needs a decision before M2 is built.** The capture cycle is serial on purpose — `takeUtterance` closes the device, transcribes, resolves, then listens again — and its doc comment states the cost: "speech during those seconds is not captured". For one-shot commands that is acceptable. For dictation it silently drops words from the middle of a prompt: somebody pausing for a breath longer than `SILENCE_HOLD` and carrying on speaking loses what they said while the previous segment was being transcribed (PRD #802 measured a 0.653 s median for Speech alone).

The serial design's reason — a second command resolving against a screen the first is still changing — does not apply while dictating, where nothing is resolved. **Proposed: while dictating, reopen the microphone before transcribing the segment just closed**, and type segments in capture order. That needs `voice/capture.rs` to hand the finished buffer off so `accepts_start` can take a new recording while the old one transcribes (today `transcribing` holds the session, and `accepts_start` refuses it). M1 measures the loss with the serial cycle first; if it is negligible in practice the change is not built, and the decision is recorded either way.

**The 30-second cap needs a dictation-specific rule.** `VOICE_CAP_DISCARDED` throws away a segment that ran to `MAX_UTTERANCE` with no pause, because a command is one to four words. A dictated paragraph is not, and discarding 30 seconds of it is the worst outcome available. Proposed: while dictating, a capped segment is **transcribed and typed**, and the report says it ran to the limit; the cost (one Speech call per 30 s of continuous speech) is spent on the user's own words, which is what the mode is for.

**Transcription artefacts** (issue trap 5). Whisper-family models emit training artefacts on near-silence, which is why transcription gates on speech density (`MIN_SPEECH`, `SPEECH_WINDOW` in `voice/capture.rs`, used by `voice/transcribe.rs`). In `Idle` an artefact reaches the Commands model and usually becomes a no-match. **In the mode it is typed into an agent's prompt** — visibly and unsent, so recoverable, but it is a new consequence for an existing weakness. M1 measures it with real silence between sentences against both Speech backends, and the result decides whether the mode needs a stricter gate than `Idle`.

### Visible indication

While dictating, the voice row shows **Typing to \<agent label\>** — the label the pane shows, sanitised through `displayText` as every other report string is — the reserved phrases ("Say “type off” to stop, “send it” to send."), and **Stop typing**. The pane's own frame gains a marker so the mode is visible where the user is looking, not only at the bottom of the window. The indicator is exposed to assistive technology (a live region for entering and leaving; the row's state in its accessible name), following the Voice button's `INDICATOR_PRESSED` precedent.

### Contract (CLAUDE.md rules 12 and 18)

Desktop-only. The new rows, the declaration's dictation target and any capture change are Rust↔webview inside the desktop binary; nothing reaches the daemon or the TUI, and typing uses the existing `sendTerminalInput` path a keystroke uses. So no `PROTOCOL_VERSION` bump and no `.breaking.md` are expected. If implementation finds a daemon change is needed after all — for example, to learn an agent's writability the desktop does not hold — rule 12's cross-version check applies and this section is corrected.

### Feature flag (CLAUDE.md rule 9)

**No new flag.** Voice control is not behind `experimental`: PRD #802 recorded that decision, and PRD #1195 followed it — the desktop binary has no flag seam (`prds/done/176-desktop-gui.md` decision 6; `features::init_and_watch` is never called there). This mode is a surface of an unflagged feature and follows that precedent.

## Testing

What rule 4 means for the desktop, as #802 and #1195 applied it: the blocking tier is Rust unit tests in `desktop/src-tauri` (run by `cargo test-fast`) plus linkage-check's tests; vitest (`desktop-web` in CI) and Playwright (`desktop-browser`, advisory) are where the panel's behaviour is proven; real-model phrase fixtures run in the credentialed lane, locally only.

| behaviour | tier | where | extends / new |
| --- | --- | --- | --- |
| on/off phrases match only as a whole utterance; "type on the tester" is not "type on"; the lists are disjoint | Rust unit | `voice/dictation.rs` tests | new |
| "type on" / "type off" are answered before the opener fast path and without a backend call | Rust unit | `voice/outcome.rs` tests (`handle_utterance_with` with a resolver that fails if called) | new |
| while dictating, no `IntentRequest` is built; reserved phrases classify in the stated order; anything else is typed whole | Rust unit | `voice/outcome.rs` tests | new |
| the new rows' `heard_as_whole`, screens and pinned row sets | Rust unit | `voice/table.rs`, `schema.rs`, `prompt.rs`, `openai.rs`, `remote.rs` pinned-set tests | extends |
| each reserved phrase appears in its row's description; lists disjoint | linkage-check | `xtask/linkage-check/src/voice_command_registry.rs` tests, with planted bad input | extends |
| entering shows the indicator and reserved set; utterances accumulate with no countdown; "send it" sends and the mode stays on | vitest | `desktop/src/components/VoiceControlPanel.test.tsx` | extends |
| every exit: off phrase, "voice off", **Stop typing**, Voice button, pane closed, agent exited, deck switched, navigation away; nothing sent on any exit | vitest | `VoiceControlPanel.test.tsx`, `VoiceControlCommands.test.tsx` | extends |
| a D5 confirmation opening ends the mode and it does not resume | vitest | `VoiceControlCommands.test.tsx` | new case |
| **Stop typing** is clickable and tabbable behind the modal pane | Playwright | `desktop/e2e/voice-control.spec.ts`, plus `AgentPaneDeckIdentity.test.tsx`'s equality | extends |
| phrasings users say route to the rows, and in-mode reserved phrases are not stolen by other rows | phrase fixtures | `desktop/src-tauri/src/voice/phrase_fixtures.toml`, run by `desktop/src-tauri/tests/voice_phrase_fixtures.rs` | extends |

**The phrase fixtures are credentialed and run on no CI runner** (they need `OPENAI_API_KEY`; CLAUDE.md rule 5). They are opt-in: an ordinary `cargo test-fast` never reaches a model and prints `SKIP:`, which nextest counts as a pass — **a SKIP is not a pass** and must be reported as unrun. `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` opts in and turns a missing credential into a failure (`desktop/src-tauri/tests/voice_phrase_fixtures.rs`'s module comment). The fixtures cover only the `Idle` half (entering); nothing in the mode reaches the model, so the in-mode classification is proven entirely by Rust unit tests.

**No L2 `tests/e2e_*.rs` test applies**, and the reason is the harness, not the change: the L2 harness drives the TUI binary through a PTY, and the TUI has no voice. No `tests/e2e_*.rs` file mentions voice today. The desktop's own driver tier (`desktop/driver/`, `desktop-driver` in CI, advisory) drives the real Tauri window against a real daemon, but it has no microphone and no transcript-injection seam, so no automated tier reaches microphone → mode → a real agent's prompt. **The real-agent check is therefore the manual walk** in `docs/develop/desktop-gui.md` (its dictation step, item 12), which this PRD extends with a mode walk against a live agent — including a long pause mid-sentence and 30 seconds of continuous speech. Whether a transcript-injection seam for the driver tier is worth building is Open Question 3.

## Success Criteria

- With an agent's pane open, "type on" enters the mode; the row names the agent and the reserved phrases; utterances are typed into that agent's prompt verbatim and unsent until "send it" or Enter.
- While the mode is on, no request reaches the Commands backend (proven by a test with a resolver that fails if called).
- Every listed exit ends the mode, sends nothing, and says why; the mode never moves to another agent.
- A missed "type off" lands visibly in the prompt, unsent.
- M1's measurements of word loss between segments and of silence artefacts are recorded, and the decisions they drive are made.
- `cargo test-fast`, rule 2's clippy, `pnpm test`, and linkage-check stay green; `PROTOCOL_VERSION` unchanged; the credentialed fixtures run locally and are named in the PR.

## Milestones

- [ ] **M1 — Measure before building the cycle.** *Not done — no microphone was available to the implementing agents; deferred to the manual walk (`docs/develop/desktop-gui.md` item 12b) and a follow-up. See the Work Log.* With the serial cycle as it is, dictate multi-sentence text with natural pauses and with deliberate long silences, against the local and the hosted Speech backends; record words lost between segments and artefacts typed. Decide, in the Work Log, whether the overlapped cycle and a stricter silence gate are built.
- [x] **M2 — The mode.** Rows, fast path, reserved-set classification, the panel state, indicator, **Stop typing**, every context-change exit, the D5 precedence, the capped-segment rule, the writability check on entry; the overlapped capture cycle if M1 decided it (it did not run, so the cycle was not built). Rust, vitest and linkage-check tests per the table.
- [x] **M3 — Fixtures, docs, changelog.** Phrase fixtures for entering and for the reserved phrases not being stolen; `docs/desktop/voice.md` (what the user says, sees and can do — rule 21), `docs/develop/desktop-gui.md` (the mode section, the manual walk step) and `docs/develop/voice-first-design.md` (the reserved set as a new class of spoken words the model is never asked about); `changelog.d/1260.feature.md` (rule 19: a user can observe this). Run the `docs-screenshots-review` skill for the user-doc change. Credentialed fixtures run locally and named in the PR.

## Risks

- **Reintroducing a removed design.** Mitigated by the "Why the old mode's two failures do not return" argument, which the M2 review should be asked to attack directly rather than take on trust.
- **Silent word loss between segments.** The largest risk specific to a mode; M1 measures it and the overlapped cycle is the answer if it is real.
- **Artefacts typed into a prompt.** Visible and unsent, so recoverable; M1 decides whether a stricter gate is warranted.
- **The reserved set grows.** Each phrase kept live is a phrase that cannot be dictated alone. The set is three classes today; adding a fourth is a decision recorded here, not a list edit.
- **A mode the user forgets is on.** Mitigated by the indicator in two places, and by every context change ending it.
- **Refused actions reporting success** (PRD #1195's `refusedRef` finding). Entering on an unwritable pane, or a send that the terminal rejects, must render only its refusal, never "Typing to …" or "Sent" beside it.

## Open Questions

1. **The exact phrase sets** for on and off. The working sets above are a starting point; the fixtures decide, and "keep typing" in particular may collide with dictated text often enough to drop.
2. **Should the mode survive the agent pane's Reader or another overlay opening over it?** The default here is "no — any change of what is on screen ends it"; a narrower rule is possible if that proves too eager in use.
3. **A transcript-injection seam for the driver tier**, so a test can drive the mode against a real daemon and a real agent's prompt without a microphone. Useful for this PRD and for #1261/#1184; it is test infrastructure with its own design (it must not be reachable in a shipped build) and is not scoped here.

## Work Log

### 2026-09-29 — Created

Written from issue #1260 by a dispatched unit, together with PRDs #1261 and #1184, around one shared voice panel state model. The design choices recorded above — whole-utterance entry and exit, accumulation with no per-utterance countdown, the submit phrase kept live by inheriting `submit_prompt`'s existing trade, no new keyboard shortcut, and the capture-cycle measurement first — are this document's; each is open to revision with a recorded reason.

### 2026-09-29 — M2 and M3 implemented; M1 deferred

M2 landed in `713cc67f` (with the test fix `4d7084a4`), M3 in the commit carrying this entry. The design above held; these are where the implementation departed from it, and each is deliberate:

- **Stop typing is not a second modal-fence marker.** The Technical Approach proposed a deliberate second `VOICE_PEER_PROPS` element with `AgentPaneDeckIdentity.test.tsx`'s equality widened to name it. The button was put **inside `.voice-row`** instead, so it inherits the row's existing single exemption exactly as the Voice button and the report's Undo do. The marker count stays at one; what grew is the set of controls reachable behind the pane, which `AgentPaneDeckIdentity.test.tsx` pins as an equality naming exactly the Voice button and **Stop typing**.
- **Exit and refusal reports are the panel's own sentences** (`dictationStopped`, `dictationRefused`, `VOICE_NOT_DICTATING`, `VOICE_CAP_TYPED` in `VoiceControlPanel.tsx`), not the rows' `report` column. Only the panel knows whose prompt it was typing to, or that no mode was on — Rust keeps no memory between utterances — so the `dictation_off` dispatch is answered on the `refusedRef` path, the way #1195's refusals are. The wording differs from the PRD's example: *"Typing mode off — the pane closed. Nothing was sent to the tester."*
- **Writability is checked from host state only.** `App.tsx` derives the pane's `inputBlocked` from `terminalInputState` (the agent's lease, status and last delivery verdict) plus whether its deck has a live link. Nothing is sent to find out and no daemon change was needed, so the Contract section's contingency did not trigger: no `PROTOCOL_VERSION` bump, no `.breaking.md`, no rule 12 cross-version run owed. A write refused anyway ends the mode with the refusal.
- **`VOICE_OFF_PHRASES` came back as a Rust list** in `voice/dictation.rs` (it had been deleted from the panel by #802's rebuild), because in the mode nothing reaches the model; linkage-check rule 14 covers it with the on/off lists, all pairwise disjoint with `SUBMIT_PHRASES` and `DICTATION_OPENERS`.
- **M1 was not measured, and the overlapped capture cycle was deferred out of this PR.** The implementing agents had no microphone, so neither the word loss between segments nor silence artefacts against either Speech backend could be recorded, and the decision M1 was to drive was not made. The serial cycle ships; the capped-segment rule (typed, not discarded) was built regardless, since it needed no measurement. The measurement is now step 12b of the manual walk in `docs/develop/desktop-gui.md` (a long pause mid-sentence, ten seconds of silence, 30 s of continuous speech), and building the overlapped cycle is follow-up work if those numbers say so. **Success criterion "M1's measurements … are recorded" is therefore unmet.**

**Phrase fixtures** (M3). Eighteen added to `phrase_fixtures.toml`: the on and off phrasings (bare, with edge politeness, and each row's `screens` refusal), switch phrases said inside a sentence that must dictate rather than switch (through the local `type` opener and through openers only the model knows), and the in-mode `VOICE_OFF_PHRASES` pinned to `voice_off` in `Idle` so the model and the list mean the same thing by them. The on/off phrasings never reach the model — every utterance that grounds to those rows is answered by the local fast path first — so those fixtures pin the pipeline's answer rather than the model's. One fixture was rephrased after a measured miss: *"tell it the mute button in the settings is broken"* came back `list_commands` (refused by grounding, so nothing ran; `voice_off` did not steal it) and was changed to the row's own *"tell it to …"* shape; the miss is a pre-existing gap in how the model reads *"tell it <statement>"*, not something this mode introduced. The fixtures' Work Log line, then, because nothing else records a credentialed run:

- **Command:** `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1 cargo test --locked -p dot-agent-deck-desktop --test voice_phrase_fixtures -- --nocapture`, against the default preset (`openai`, `gpt-5-mini`), 2026-09-29. Through `nextest` the same run is killed by the default 180 s slow-timeout before it finishes, so it has to be `cargo test`; `desktop-gui.md` step 13 now says so.
- **Result:** the final run passed **143 of 149**; all **18** new fixtures passed in both full runs after the rephrasing above. The six failures are not this PRD's — `choose-deck-in-the-open-dialog`, `choose-deck-over-a-live-form`, `choose-deck-by-its-label`, `open-new-agent-on-the-remote-deck`, `close-new-agent-button-label` and `submit-unavailable`. Measured at the branch point `15c514ff`, with neither this PRD's rows nor its fixtures, the same manifest failed 9 of 131, including every one of those but `submit-unavailable`, which passed there and failed in two of three branch runs. So it was sampled on its own, 30 times against each table: **22/30 with this PRD's rows and 23/30 without them**, every miss answered `no_match` — a pre-existing ~25% flake on *"okay, send it please"* said on the dashboard, not a shift from the new rows. The deck ones are model routing that moved with #1045's deck → daemon wording (`29159ebb`), which did not re-run these fixtures; they need a description fix and an owner, and are reported rather than fixed here.
- **Fixed on the way:** `deck-hidden-back-to-deck` and `deck-hidden-open-deck` failed on every run because the runner checked the refusal for the word "deck", which #1045 renamed to "Daemons"; the runner now compares against `schema::DECK_HIDDEN_HINT` itself and both pass.

