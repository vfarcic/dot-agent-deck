# PRD #1541: Voice control of the open agent's prompt — interrupt, clear, scratch that

**Status**: Complete
**Completed**: 2026-10-04
**Priority**: Medium
**Created**: 2026-10-03
**Issue**: [#1541](https://github.com/vfarcic/dot-agent-deck/issues/1541)
**Related**: [#1542](https://github.com/vfarcic/dot-agent-deck/issues/1542) (answering the agent's questions by voice), [#1402](https://github.com/vfarcic/dot-agent-deck/issues/1402) (closing the agent from its own screen), [#1497](https://github.com/vfarcic/dot-agent-deck/issues/1497) (hearing the agent's turns)

## Problem Statement

With voice control on, an agent's pane in the desktop app accepts four things by voice: type into the prompt, send it, turn typing mode on and off, and close the pane (`screens = ["agent"]` rows in `desktop/src-tauri/src/voice/commands.toml`). Nothing sends any key other than the typed words and Enter. So, hands-free, a user cannot:

- **stop an agent mid-turn** when it heads the wrong way — the only voice command that stops anything terminates the agent, from the dashboard, behind a confirmation;
- **wipe a prompt** they dictated badly before sending it;
- **take back one misheard sentence** without wiping the rest of a long dictation.

The last one is a gap the user docs already admit to: when "type off" is misheard, the words are typed into the prompt and `docs/desktop/voice.md` says to "press **Stop typing** and delete them" — by hand.

## Solution Overview

Three voice commands that work in an agent's pane:

1. **Interrupt** — stop the agent's current turn, leaving the agent running and its session intact.
2. **Clear the prompt** — remove everything in the agent's unsent prompt.
3. **Scratch that** — remove only the words the app typed last, when they are still the end of the prompt.

Each is delivered as the keys that do that job **for the agent in the pane**. Agents differ: the key that interrupts one may do something else in another, and some keys are harmful when repeated — pressed twice, `Ctrl+C` quits Claude Code. So the per-agent keys live in one place, a blind `Ctrl+C` is never used, and an agent with no safe key for a command is refused that command with a reason and named in the docs (CLAUDE.md rule 20).

## What the user sees

- In an agent's pane, saying "interrupt" (or another phrase chosen in M1) stops the agent's turn; the row beside the Voice button says what happened, e.g. *Interrupted Desktop implementation.*
- Saying "clear the prompt" empties the agent's unsent prompt; the row says so.
- Saying "scratch that" right after dictating removes those words and nothing else. If the prompt changed since — the user typed, or the agent changed — nothing is removed and the app says why.
- On an agent that cannot support a command, the command runs nothing and the app names what is missing, e.g. *Pi has no key to clear its prompt.*
- "What can I say?" lists the new commands under the agent screen.

## Scope

### In scope

- The three commands above, on the desktop app's agent screen, for every agent in `src/agent_registry.rs` that can support them.
- Per-agent key definitions in one place, with their verification against each agent's current release recorded.
- The phrases, chosen so they do not collide with existing ones (below).
- Whether each command works during [typing mode](../docs/desktop/voice.md#typing-mode), which today accepts only a handful of phrases said on their own.
- Tests, user docs and developer docs.

### Out of scope

- **Answering the agent's questions** (permission prompts, menus) — that is [#1542](https://github.com/vfarcic/dot-agent-deck/issues/1542), which depends on knowing what is being asked.
- **Terminating the agent** from its own screen — [#1402](https://github.com/vfarcic/dot-agent-deck/issues/1402).
- **Scrolling** the pane — covered elsewhere.
- **Switching agents from inside a pane** — considered and dropped: the agent list is not on screen there, so it would be choosing blind.
- **The TUI**, which has no voice control; its keyboard already reaches the pane directly.

## Design decisions and constraints

- **Words must not collide.** "Stop the planner" already means *terminate* (with confirmation), and #1402 may add "close this agent" on the agent screen. "Cancel", "never mind" and "none" already close a choice list, and "close"/"cancel" close the New agent dialog. Interrupt needs phrases of its own, and M1 decides what a bare "stop" in an agent's pane resolves to — including refusing it as ambiguous.
- **Never a blind `Ctrl+C`, and nothing that harms the agent when repeated.** A misheard or repeated command must not quit the agent or open an unrelated menu. Where an agent's key is unsafe to send twice, the app must not send it twice for one utterance.
- **Per-agent data lives in one place.** `src/agent_registry.rs` is "the single place per-agent data lives". Whether the keys are read there by the desktop, or served by the deck so a remote deck of another version answers for its own agents, is an M1 decision under CLAUDE.md rule 18.
- **"Scratch that" refuses rather than guesses.** It only removes text the app itself typed, only while that text is still the end of the prompt, and only in the same pane and the same agent. Agents whose input editor rewrites what it is given (for example collapsing a paste into a placeholder) may make this impossible to do safely; such an agent is refused and documented.
- **Visible outcome, staleness rules unchanged.** Each command reports what it did, and follows the existing voice rules: nothing runs if the pane on screen changed, a confirmation is open, or the agent was replaced while the app was working out what was said.
- **Where phrases are decided.** Today, send phrases and "type on/off" are matched on this machine and never reach the Commands service. M1 decides whether the new phrases join that local set, and `docs/desktop/voice.md`'s "What is sent where" is updated to match.

## M1 decisions (recorded 2026-10-04, with the user)

These answer the M1 milestone and the open questions below. Where they differ from earlier sections of this document, these win.

1. **Experimental flag (CLAUDE.md rule 9): no.** The commands ship visible, like the rest of voice control.
2. **Typing mode decides what a word means, and all three commands live only in typing mode.** With typing mode on, the user is talking *to the agent about its prompt*, so prompt words are commands there. With it off, the user is running *the app*, and the agent screen keeps its app commands. A phrase belongs to exactly one mode. Commands that end an agent or close something (`stop_agent`, and whatever #1402 adds) are **not** reachable in typing mode until the user says "typing off", which is already true today because typing mode types everything it does not recognise.
3. **Matching is local and whole-utterance.** The phrases join the on-machine lists in `desktop/src-tauri/src/voice/dictation.rs` and are matched by `dictation_intercept` the way "send" and "typing off" are: the whole utterance, less an edge politeness word, must equal a listed phrase. "Scratch the last prompt" said alone is a command; "we should work on the scratch feature" is typed; a command word inside a sentence never fires; and "type …" (the dictation opener) always types what follows. Nothing new is sent to the Commands service. `docs/desktop/voice.md`'s "What is sent where" and the in-app disclosure are updated to say so.
4. **Phrases (typing mode only).**
   - **Interrupt:** "interrupt", "interrupt it", "interrupt that", "stop", "stop it", "stop that". A bare "stop" in typing mode therefore interrupts; "stop typing" and "stop listening" keep their meanings (distinct whole phrases).
   - **Clear the prompt:** "clear the prompt", "clear prompt", "clear it", "clear all", "clear everything", "delete everything".
   - **Scratch that:** "scratch that", "scratch it", "scratch the last part", "scratch the last sentence", "scratch the last prompt", "delete that", "undo that".
   - **Outside typing mode**, the interrupt-only, clear and scratch phrases ("interrupt", "clear the prompt", "scratch that", …) run nothing and the row says to say "typing on" first. A bare "stop" outside typing mode is left to #1402, which was updated on 2026-10-04 to make it end the open agent behind the stop confirmation; until #1402 lands it keeps today's answer.
5. **Interrupt asks for no confirmation.** In every supported agent it ends the turn and keeps the agent, its session and any draft typed mid-turn, so a mishearing is recoverable.
6. **Repeat guard.** A repeated interrupt reaching an idle agent is harmful (it opens Rewind in Claude Code, the transcript browser in Codex, the Session Tree in Pi on an empty prompt, and clears the draft in Claude Code). So interrupt is sent only while the agent's status says it is working, and a second interrupt to the same pane within a few seconds of the first is refused rather than sent.
7. **Where the keys live (rule 18): the deck.** The per-agent keys are defined in `src/agent_registry.rs` and served by the daemon as an additive optional field on `AgentRecord` (no `PROTOCOL_VERSION` bump, the `cli_name` precedent of #856), so a deck answers for the agent versions on its own host. The desktop sends the bytes over the existing terminal-input path. A deck too old to send the field gets the command refused with a reason.
8. **Undo for clear: yes, when known.** The existing Undo button re-types the cleared text only when every character of the prompt came from voice since the last send, clear or interrupt in that pane; any keyboard input in the pane disables it, and the row then says the clear cannot be undone.
9. **Devin: unsupported for all three commands**, named in `docs/desktop/voice.md` (rule 20). It is logged out on the box where the keys were measured, so none of its keys could be verified; revisit when someone measures it logged in.

### Verified per-agent key table

Measured 2026-10-03 in a private tmux server, cheap models, versions from `--version`.

| | Claude Code 2.1.289 | Codex 0.160.0 | OpenCode 1.18.34 | Pi 0.87.1 | Devin 3000.11.3 |
| --- | --- | --- | --- | --- | --- |
| Interrupt | `ESC` once | `ESC` once | `ESC`, ~0.3 s pause, `ESC` (the first only arms "esc again") | `ESC` once | unsupported (not measured) |
| Clear | `Ctrl+U` (0x15), one per wrapped screen row; a single write of 64 or more is ignored, so send in writes of at most 32 | `Ctrl+U`, one per line | `Ctrl+U`, one per line | `Ctrl+U`, one per line | unsupported (not measured) |
| Delete one character | `DEL` (0x7f) | `DEL` | `DEL` | `DEL` | unsupported (not measured) |
| Paste collapse of an unbracketed write | more than 800 characters becomes `[Pasted text #N]` | more than 1000 becomes `[Pasted Content …]` | never | never | unknown |

Extra `Ctrl+U` presses on an empty prompt are harmless in all four measured agents, so clear may over-count. `Ctrl+C` is never used: on an empty prompt it quits Codex and OpenCode outright. "Scratch that" removes the last voice write with `DEL` × its character count, and refuses a write longer than 800 characters, where Claude Code and Codex may have collapsed it.

## Milestones

- [x] **M1 — Decisions recorded.** (See "M1 decisions" above.) The rule 9 experimental-flag answer; the phrases for each command and what a bare "stop" means in a pane; the verified per-agent key table (Claude Code, OpenCode, Codex, Pi, Devin) with the agent versions checked and the unsupported cells named; where the key definitions live (desktop vs deck, rule 18); which commands work during typing mode; whether "clear the prompt" offers Undo by re-typing the cleared text when the app knows all of it; local vs Commands-service matching.
- [x] **M2 — Interrupt.** The voice row, the per-agent keys, the outcome reports and refusals; works on every agent M1 marked as supported. Done: deck-served keys (`AgentRecord.prompt_keys`, `src/agent_registry.rs`), exact per-operation allowlist and pause budgets at the desktop seam, interrupt sent only while the agent is working, checked again immediately before each key is handed to the daemon, latched until a new turn is seen, one pending prompt command per pane, cancelled with typing mode.
- [x] **M3 — Clear the prompt.** Same shape as M2. Done: Ctrl+U presses (32 per line, 64 per wrapped row for Claude Code in writes of 32 a deck-served 1 s apart), Undo only when the app has seen the prompt emptied and nothing but voice typed since, bound to a prompt revision any keyboard input invalidates.
- [x] **M4 — Scratch that.** Tracking what the app last typed per pane, the "still the end of the prompt" check, and the refusals when it cannot be sure. Done: per-pane voice-write history per agent incarnation; scratch re-checks the prompt revision and the burst of writes around the last one just before sending DEL × its length, and refuses over the 800-character floor, on combining or astral characters, and when the writes around it may have been read as one paste.
- [x] **M5 — Tests.** Phrase fixtures and `voiceActions` unit tests for every command and refusal; desktop tests for the outcome row; a real-agent test (lane 2, CLAUDE.md rules 4 and 5) that interrupts a cheap-model agent mid-turn and confirms it is still running and accepts the next prompt. Done: phrase fixtures (typing mode on/off), `voiceActions` and panel vitest suites (incl. real-bridge queue tests), registry/DTO/protocol unit tests, and lane-2 `prompt/voice-keys/001–005` (Claude Code on Haiku, Codex, OpenCode) run locally. Pi has no lane-2 coverage on the box it was built on (no credential route for Pi in the harness); Devin is unsupported.
- [x] **M6 — Docs and release notes.** `docs/desktop/voice.md` (the agent-screen row of the command table, the typing-mode list, "What is sent where", the coverage of each agent), `docs/develop/voice-first-design.md` for the per-agent keys, the docs-screenshots-review skill, and a changelog fragment (rule 19). Done: `docs/desktop/voice.md`, `docs/develop/voice-first-design.md`, `changelog.d/1541.feature.md` and `1541.bugfix.md` (the Enter-settle fix found along the way), `settings-voice-desktop.png` regenerated.

## Risks

- **Agents change their keys.** A key that interrupts today may do something else after an agent upgrade. Mitigation: the key table records the versions it was verified against, and the lane-2 test catches a regression for the agents it covers when it is run.
- **Misrecognition.** A misheard "interrupt" stops work the user wanted. It is recoverable — the agent keeps its session — but M1 should weigh requiring a fuller phrase.
- **Clear is destructive.** A misheard "clear" loses a long dictation. M1 decides whether Undo is offered.

## Open questions

1. Should interrupt ask for confirmation, or rely on being recoverable?
2. Does a bare "stop" in an agent's pane mean interrupt, or is it refused as ambiguous with terminate?
3. Do the key definitions belong to the deck (served per agent by the daemon) or to the desktop build?

## Success criteria

- A user can interrupt, clear and scratch by voice on every agent M1 marked as supported, without touching the keyboard.
- No voice command in this PRD can quit an agent or open an unrelated menu, including when said twice.
- Every unsupported agent/command pair is refused with a reason on screen and named in `docs/desktop/voice.md`.
