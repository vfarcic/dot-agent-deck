# PRD #1541: Voice control of the open agent's prompt — interrupt, clear, scratch that

**Status**: Draft
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

## Milestones

- [ ] **M1 — Decisions recorded.** The rule 9 experimental-flag answer; the phrases for each command and what a bare "stop" means in a pane; the verified per-agent key table (Claude Code, OpenCode, Codex, Pi, Devin) with the agent versions checked and the unsupported cells named; where the key definitions live (desktop vs deck, rule 18); which commands work during typing mode; whether "clear the prompt" offers Undo by re-typing the cleared text when the app knows all of it; local vs Commands-service matching.
- [ ] **M2 — Interrupt.** The voice row, the per-agent keys, the outcome reports and refusals; works on every agent M1 marked as supported.
- [ ] **M3 — Clear the prompt.** Same shape as M2.
- [ ] **M4 — Scratch that.** Tracking what the app last typed per pane, the "still the end of the prompt" check, and the refusals when it cannot be sure.
- [ ] **M5 — Tests.** Phrase fixtures and `voiceActions` unit tests for every command and refusal; desktop tests for the outcome row; a real-agent test (lane 2, CLAUDE.md rules 4 and 5) that interrupts a cheap-model agent mid-turn and confirms it is still running and accepts the next prompt.
- [ ] **M6 — Docs and release notes.** `docs/desktop/voice.md` (the agent-screen row of the command table, the typing-mode list, "What is sent where", the coverage of each agent), `docs/develop/voice-first-design.md` for the per-agent keys, the docs-screenshots-review skill, and a changelog fragment (rule 19).

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
