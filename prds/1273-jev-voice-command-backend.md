# PRD #1273: Jev as an optional command model for desktop voice control

**Status**: Draft — **blocked on TypeSafe early access.** No Jev key is available yet, so no milestone can start. The design below was settled in discussion on 2026-09-24; M1 is a go/no-go measurement, and nothing after it is built unless M1 passes.
**Priority**: Low
**Created**: 2026-09-24
**Depends on**: [PRD #802](done/802-desktop-voice-control.md) (desktop voice control, shipped) and a TypeSafe early-access key.

## Problem Statement

Desktop voice control ([PRD #802](done/802-desktop-voice-control.md)) turns an utterance into an action in two stages: **Speech** transcribes audio to text, and **Commands** resolves that text against the command table (`desktop/src-tauri/src/voice/commands.toml`) and live state. Commands has two backends today, selected by `[voice.intent] backend`: `openai_compatible` (the default, `gpt-5-mini` with reasoning suppressed) and `anthropic` (`claude-haiku-4-5`, strict tool-use). Both are chat models forced into a closed-set answer. #802 measured the Anthropic shape at a **~0.9 s median** per utterance, which is most of what a user waits for after they stop speaking.

[Jev](https://docs.typesafe.ai/), released by TypeSafe AI on 2026-09-15, is a different kind of model: a "System One" decision model that returns no text at all, only typed answers to choice, yes/no and score questions, with probabilities. TypeSafe claims 70–500 ms latency, and pricing is $0.042 per million input tokens with free output. That shape matches #802's own design principle unusually well — **the model returns a situation; the app renders the sentence** — because Jev *cannot* return anything but a situation.

Users cannot select it today.

## Solution Overview

Add Jev as a **third Commands backend**, chosen in the same settings panel as the other two.

- **Same fields.** It uses the existing `[voice.intent]` endpoint, model and key fields. Keys are stored per stage (`SecretId::VoiceIntent`, `desktop/src-tauri/src/secrets.rs:104`), not per provider, so no new secret and no new settings field is needed. The only new setting value is the `backend` token (working name `typesafe`), because the backend selects the request/response protocol and Jev's API is neither chat-completions nor Anthropic messages.
- **Top result only.** Like the existing backends, the app acts on the single top answer. Jev's probabilities are not used in this PRD.
- **Every question is a choice.** One request carries the transcript, the live state and a set of questions Jev answers in parallel:
  - **which action?** — a choice over every command `id`, plus `none` (the "none of these" escape #802 calls most of the safety);
  - **one question per param kind**, built mechanically from `ParamKind` (`desktop/src-tauri/src/voice/table.rs:98`, which has exactly two kinds today):

    | param kind | Jev question | options come from |
    | --- | --- | --- |
    | `agent_ref` | which agent? | the distinct names the model is shown for live agents today (label, role, CLI name — `voice/prompt.rs`'s `state`), each offered once; the chosen name then goes through `resolve_agent_ref` unchanged |
    | `spoken_prefix` | where does the text to type start? | **the transcript itself**, cut after each word, up to and including the whole utterance — generated per utterance, never a predefined list |

  Jev answers every question on every request; the app ignores the answers that do not belong to the chosen action. Output is free, so this costs nothing extra.
- **Speech is untouched.** Transcription stays its own stage with its own fields and model. Selecting Jev means a user has two different keys (Speech and Commands) rather than pasting one OpenAI key into both — a lost convenience, not a new field.
- **No per-command code.** A command row in `commands.toml` still needs no backend-specific code: the Jev adapter derives its questions from the table and from `ParamKind`, exactly as the schema generator derives the strict tool schema today.

### Why dictation's prefix is a choice, and why that is not a phrase list

`dictate_to_agent` asks the model for `prefix`: the words that *introduce* the dictation (*"let's write a prompt"* in *"let's write a prompt run the login tests"*), which `voice::dictation::strip_opening` verifies against the transcript before anything is typed. Jev cannot write words, so the adapter turns the prefix into a choice among the transcript's own leading word sequences:

For *"alright so could you get it to run the login tests"*: `alright` · `alright so` · `alright so could` · … · `alright so could you get it to` ← the answer · … · the whole sentence.

The whole sentence stays an option on purpose: it is how Jev says "the user spoke only an introduction and there is nothing to type". `strip_opening` accepts it and leaves no text, and the validation that already runs after `strip_opening` (`voice/outcome.rs`, the `SpokenPrefix` arm) refuses that as `ParamUnresolved` — the same outcome a chat-model backend gets today when its prefix is the whole utterance.

Nothing is predicted in advance — the options are every way of splitting **this** utterance into "introduction" and "text to type", so any opening a user could say is among them because they said it. Two consequences worth stating:

- **It is the same job the current backends do.** They must already return a prefix that `strip_opening` accepts, i.e. one of these splits; they spell it out and Jev picks it by index.
- **It strengthens the fidelity property `dictation.rs` exists to hold** — *the app supplies the typed text; the model may only say where it starts*. Every option is a transcript slice by construction, so a model answer naming words the user did not say is unrepresentable rather than merely refused. `strip_opening` stays in the path regardless.

Whether Jev picks the **right** split (*"ask it to"* and not *"ask it to summarise"*) is an accuracy question, and M1 answers it.

## Scope

### In Scope

- A `typesafe` value for `[voice.intent] backend` with its own preset endpoint and model, selectable in the Voice settings panel.
- A Jev protocol implementation beside the existing ones in `desktop/src-tauri/src/voice/` (`remote.rs`, `openai.rs`), reached from `resolver_for` (`voice/resolver.rs:203`).
- Question generation from the command table and `ParamKind`, and mapping Jev's answers onto the existing nine outcomes in `voice/outcome.rs`.
- A way to run the phrase fixtures against Jev for M1's measurement and for later regression checks.
- Developer docs in `docs/develop/desktop-gui.md`, and a changelog fragment.

### Out of Scope

- **Using Jev's probabilities** — confidence thresholds, "did you mean…?" suggestions, showing confidence in the report. A possible follow-up once the top-result backend exists.
- **Changing the default backend.** `openai_compatible` stays the default so one key runs the whole feature; Jev is opt-in.
- **Self-hosting.** At the time of writing Jev is API-only (no published weights, no on-prem or VPC option). A local decision-model tier (e.g. Laya, Kev-9B, AnyJev) belongs with #802's deferred local-intent work, not here.
- **Speech-to-text changes.**
- **Daemon or protocol changes.** This is desktop-only (see Cross-version).

## Technical Approach

### Mapping Jev's answers onto the existing outcomes

| Jev's answer | outcome |
| --- | --- |
| action = `none` | `NoMatch` |
| action whose `screens` excludes the current screen | `Unavailable` (the app computes availability from `screens`, as validation already does) |
| action with an `agent_ref` param, `resolve_agent_ref` matches the chosen name to one live agent | `Dispatch` |
| action with an `agent_ref` param, the chosen name matches more than one live agent | `ParamAmbiguous` |
| action with an `agent_ref` param, the chosen name matches no live agent | `ParamUnresolved` |
| action with an `agent_ref` param and no live agent to offer | `ParamMissing` (the question has no options, so the adapter does not ask it and reports the param absent) |
| action with a `spoken_prefix` param, `strip_opening` accepts the chosen split and text remains to type | `Dispatch` |
| action with a `spoken_prefix` param, the chosen split is the whole utterance | `ParamUnresolved` |
| request failed, timed out, or the reply does not parse | `ResolutionFailed` |

Three existing outcomes behave differently under Jev, and the PRD accepts that rather than hiding it:

- **`ParamAmbiguous` for agents narrows to shared names.** Today the model returns the name it heard and `resolve_agent_ref` (`voice/outcome.rs:734`) matches it against live agents, reporting ambiguity when it matches more than one — either because two agents answer to the same name (the `open-agent-ambiguous-name` fixture, *"zoom coder"* against two agents both shown as *Atlas* with role `coder`) or because a partial name loosely matches several distinct ones. Because Jev's options are names rather than agents, and the chosen name goes through `resolve_agent_ref` unchanged, a name two agents share is still reported as ambiguous, so that fixture stays passable and M1's parity bar needs no exception for it. What is lost is the partial-name case: unless the partial name is itself some agent's whole name, it is not among the options, so Jev picks one of the complete names. Recovering it would need the probabilities, which are out of scope.
- **`UnknownAction` becomes unreachable** because the action is a closed choice. It stays a variant for the other backends.
- **`ParamMissing` / `ParamUnresolved`** narrow to the rows in the table above: `ParamMissing` when there is no live agent to offer, and `ParamUnresolved` when the chosen prefix is the whole utterance or the chosen name matches no live agent. That last row is not dead: a label the model is shown is not always a name `resolve_agent_ref` answers to — an agent with no display name and no role is shown as `Agent N` (`display_label`), which is not among the names the resolver matches (`spoken_names`) — so M3 either builds the options from the names the resolver matches or keeps that refusal. **An agent that exits while Jev is answering is not caught at resolution:** `desktop_voice_resolve` (`desktop/src-tauri/src/lib.rs`) reads one snapshot before the request and `handle_utterance` validates against that same snapshot. That window exists for the current backends too; this PRD neither widens nor closes it, and closing it (a fresh existence check at dispatch) would be its own change.

### The `description` column is shared

Every row's `description` is the prompt for every backend. If Jev needs different phrasing to pass the fixtures, a wording change moves results for the chat-model backends too. **Accepted:** if M1 shows Jev needs it, a Jev-specific description column is an acceptable addition to the table. Record it in this PRD as a decision when it happens rather than letting two phrasings drift silently.

### Settings, and an older build reading `typesafe`

`IntentBackend::from_str_lossy` (`desktop/src-tauri/src/settings.rs:1041`) folds any unknown token to the default (`openai_compatible`). The existing `remote` arm exists precisely because such a fold once risked putting one provider's key in an `Authorization` header addressed to another. An older desktop build opening a settings file that says `typesafe` will fold the same way. Whether that sends the TypeSafe key to `api.openai.com` depends on whether the saved endpoint survives the fold (`IntentSettings`'s `Deserialize` keeps a stored endpoint over the preset's). **Decision:** verify it by test, running a build taken from `main` against a settings file written by this branch, and record the result here. If the key can reach another provider, fix it before this ships.

### Data handling

Selecting Jev sends transcripts and live agent names to TypeSafe, whose data-retention policy was not found at the time of writing. **Decision:** Jev is an opt-in choice and users who select it take responsibility for that choice. The settings panel and docs disclose where the data goes, the same way they do for the existing providers — no additional gate.

### The phrase fixtures and a backend switch

`desktop/src-tauri/tests/voice_phrase_fixtures.rs` drives `phrase_fixtures.toml` and **deliberately offers no backend switch**: they are authoritative for the shipping default only, because "a green run against another model would prove less than it appears to" (`docs/develop/desktop-gui.md`). M1 needs to run them against Jev. The design must keep that property: the default run stays exactly as it is, and a Jev run is an explicit, separately-named invocation whose result is recorded as a comparison, not as authority for the default.

### Cross-version and flags

- **CLAUDE.md rule 12 does not apply**: nothing here touches the daemon, the TUI↔daemon protocol, orchestration or hooks.
- **CLAUDE.md rule 9**: no experimental flag. The desktop binary never ships behind it (PRD #176 decision 6, repeated in #802).
- **CLAUDE.md rule 19**: a user can observe the change (a new option in settings), so it gets a changelog fragment.

## Success Criteria

- **Go/no-go (M1):** on the phrase fixtures, Jev's pass rate matches the current default backend's pass rate. Both are measured on the same box against the fixture set as it stands when M1 runs — the set grows as commands are added, so the bar is relative and this PRD pins no count. Below that bar, the only thing Jev offers is speed, and the PRD stops at M1 with the measurement recorded.
- A user can select Jev in the Voice settings panel, paste a TypeSafe key into the existing Commands key field, and use every shipped command — including dictation with an opener no fast-path list contains — without any other change.
- A command added to `commands.toml` works under Jev without Jev-specific code.
- An older desktop build reading a settings file with `backend = "typesafe"` never sends the TypeSafe key to another provider.
- Latency is measured and recorded against the existing backends on the same box.

## Milestones

- [ ] **M1 — Go/no-go measurement.** With an early-access key: confirm Jev's API against TypeSafe's current docs, build a throwaway adapter (action choice + both param-kind questions, transcript-prefix options), and run all phrase fixtures against Jev and against the current default on the same box. Record accuracy per fixture (dictation-prefix cases called out separately) and latency. **Stop here if the pass-rate bar is not met.**
- [ ] **M2 — `typesafe` backend in settings.** New `IntentBackend` token and preset, selectable in the Voice settings panel with the data-handling disclosure, using the existing endpoint/model/key fields.
- [ ] **M3 — Jev protocol implementation.** Production adapter in `voice/`, question generation derived from the command table and `ParamKind`, answers mapped onto the existing outcomes as in the table above, with unit tests against recorded Jev responses (no network in `cargo test-fast`).
- [ ] **M4 — Older-build safety verified.** Run a build from `main` against a settings file written by this branch; record whether the TypeSafe key can reach another provider, and fix it if it can.
- [ ] **M5 — Fixtures runnable against Jev.** An explicit, separately-named Jev fixture run that leaves the default run unchanged, documented beside the existing command; run it green locally and record the result (it runs in no CI job, CLAUDE.md rule 5 lane 2).
- [ ] **M6 — Docs and changelog.** `docs/develop/desktop-gui.md` covers the third backend, its question mapping, the outcomes that behave differently under it, and the manual smoke walk's dictation step under Jev; note for #802's D12 (the future user-facing page) that Commands has three protocols; changelog fragment.

## Risks

- **Early access may not arrive, or the API may change.** Jev is v1.13 from a startup that came out of stealth on 2026-09-15. Mitigation: nothing is built before M1, and M1 starts by checking the API against the docs of the day.
- **Jev's documented weaknesses fall on this workload.** TypeSafe's own docs list literal reading, indirection, irrelevant context and adversarial content among Jev 1.13's failure modes. Spoken commands are indirect by nature, and agent names in the live state are not fully trusted input. Mitigation: M1 measures the first two directly; validation and `strip_opening` stay in the path for the rest.
- **The latency win may be smaller end to end than the intent-stage number.** Speech is a separate round trip that Jev does not shorten. Mitigation: M1 records the intent-stage latency; the smoke walk is where the end-to-end difference is felt.
- **Shared descriptions.** See Technical Approach; mitigation is the fixtures plus the accepted fallback of a Jev-specific column.
- **Lost `ParamAmbiguous` for partial agent names.** A partial name that loosely matches several agents is resolved by Jev's pick rather than by asking; a name two agents share is still reported as ambiguous (Technical Approach). Mitigation: accepted for this PRD; the probabilities follow-up can restore it.

## Open Questions

1. What is Jev's exact request shape for supplying state and choice options, and its option-count and context limits? A third-party analysis (TrueFoundry) reports a 255-option cap per choice question, which would cover any spoken utterance and any realistic agent count; that figure is not from TypeSafe's own docs, so confirm it in M1.
2. Does forcing every question on every request (including the prefix question when the action is not dictation) measurably hurt the action answer's accuracy, compared with asking only the action question? M1 can measure both.
3. What model identifier does the preset pin (`jev-1.13` or a moving alias)? #802 re-checks request shapes whenever the model moves, so a pinned id is preferred.

## Work Log

### 2026-09-24 — Created

Designed in discussion before any key was available. Decisions: reuse the existing Commands fields; top result only; every param kind becomes a choice question, with dictation's prefix chosen among the transcript's own leading word sequences; M1 is a go/no-go measurement with parity to the current backends' pass rate as the bar; the older-build settings fold is verified by testing against a build from `main`; a Jev-specific description column is acceptable if the fixtures require it; data handling is the user's choice and responsibility, disclosed like the other providers.
