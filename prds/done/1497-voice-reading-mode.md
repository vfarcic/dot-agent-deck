# PRD #1497: Voice "reading on" — hear a short summary of the open agent's turns

**Status**: Complete — 2026-10-08
**Priority**: Medium
**Created**: 2026-10-02
**Issue**: [#1497](https://github.com/vfarcic/dot-agent-deck/issues/1497)
**Builds on**: [PRD #802](https://github.com/vfarcic/dot-agent-deck/issues/802) (voice control for the desktop app), PR #1451 (voice typing mode, the model for an on/off voice mode scoped to the open agent), [#714](https://github.com/vfarcic/dot-agent-deck/issues/714) / [#1359](https://github.com/vfarcic/dot-agent-deck/issues/1359) (turn-end and failed-turn signals from Claude Code's hooks and Codex's session log)
**Related**: [#1493](https://github.com/vfarcic/dot-agent-deck/issues/1493) (accurate Codex statuses; reading relies on knowing when a Codex turn ends), [#1495](https://github.com/vfarcic/dot-agent-deck/issues/1495) (naming agents naturally by voice)

## Problem Statement

Voice control lets the user talk to an agent but not listen to it. Typing mode ("typing on") puts the user's words into the open agent's prompt, but the only way to learn what the agent did is to look at its terminal. That defeats the point of a voice-first workflow: the user who is talking to the deck from across the room, or while looking at something else, has no way to hear the result.

Reading the agent's output aloud is not the answer. A terminal agent's screen is mostly redraws, spinners, menus, diffs and tool output, and even its clean final reply is usually far too long to listen to. What the user wants to hear is what a colleague would say: "it finished, the tests pass, and it changed two files", or "it is asking permission to run cargo publish".

## Solution Overview

A voice mode, **reading**, scoped to the open agent, mirroring typing mode:

1. **"reading on"** while an agent's pane is open starts it. From that moment on, and only from that moment on, the app speaks:
   - a **one-or-two-sentence summary of each turn the agent finishes**;
   - a **permission prompt** as soon as the agent raises one (what it wants to do), because the user has to act on it;
   - an **error or quota block** as soon as it happens.
2. **The summary is written by the model voice control already uses** (the configured OpenAI-compatible or Anthropic connection under `desktop/src-tauri/src/voice/`), given the agent's **final reply** for that turn and an instruction to summarise it briefly for listening. No second provider and no new key.
3. **The source is the agent's own turn-end data, never its terminal output.** For example, Claude Code's hook payloads carry `last_assistant_message` (`src/hook.rs`), and Codex's session log records `last_agent_message` on `task_complete` (`src/codex_rollout_tail.rs`, `src/quota_signals.rs`). The daemon already receives or reads both. The daemon exposes the final reply of a turn to clients; the desktop does not scrape anything.
4. **Speech** comes from the configured provider's text-to-speech when it offers one, or from the operating system's built-in voice otherwise.
5. **"reading off"**, closing the agent's pane, or turning voice off ends it. **"stop"** or **"quiet"** cuts off what is being spoken without turning reading off.

The agent itself is never touched: no prompt is injected, nothing is written into its project, and its behaviour, tokens and permissions are unchanged.

## What the user sees

- With an agent's pane open and voice on, the user says "reading on". A visible indicator on the pane (like typing mode's) says reading is on, and the app confirms by speaking "Reading on".
- When the agent finishes a turn, the app speaks a short summary, e.g. "The tester finished: all 42 tests pass, nothing changed."
- When the agent asks for permission, the app speaks it immediately, e.g. "The coder is asking to run cargo publish."
- "stop" or "quiet" silences the current sentence. "reading off" ends the mode, as do closing the pane and "voice off".
- The first time, reading must be turned on in **Settings → Voice**, which explains that the agent's replies are sent to the configured model to be summarised (and to the speech service, if that is the provider's).
- Where the agent cannot provide turn-end data, "reading on" says so in plain words instead of silently doing nothing (CLAUDE.md rule 20).

## Scope

### In scope

- The **reading** voice mode: `reading_on`, `reading_off` and an interrupt row in `desktop/src-tauri/src/voice/commands.toml`, available only while an agent's pane is open, plus the visible indicator.
- Turn-end summaries, immediate permission-prompt and error announcements, produced from the agent's final reply and signals, never its terminal output.
- A daemon-side way for a client to receive an agent's turn-end reply (the final reply and its turn), gated on a daemon capability per CLAUDE.md rule 18.
- Summarisation through the existing voice model connection, with a dedicated, bounded prompt.
- Text-to-speech through the provider when available, otherwise the OS voice, with the microphone kept from hearing the app's own speech.
- The Settings opt-in and its privacy explanation.
- Per-agent coverage, with the gaps documented (rule 20).
- User docs and tests.

### Out of scope

- Reading output for an agent that is not open, and reading several agents at once.
- Reading the agent's full replies, its terminal output, or tool output aloud.
- Voice "conversation" (asking follow-up questions about what was read).
- The TUI. It has no voice; record that in this document per CLAUDE.md rule 22.
- Changing the agent's behaviour in any way (no injected prompts, no files written by the agent).

## Design decisions and constraints

- **D1: Never read stdout.** The summary is built from the agent's own turn-end data. Terminal output is noise for this purpose, and screen scraping is fragile.
- **D2: The deck summarises, not the agent.** An injected "write a summary to a file" instruction was considered and rejected (2026-10-02): it triggers file-write permission prompts on restricted agents, agents forget it over long sessions and after `/clear` or compaction, and it costs tokens and adds fake tool calls to the card.
- **D3: Only the open agent, only from "reading on" onward.** Nothing already on screen is read, and there is no backlog.
- **D4: Opt-in.** Reading sends agent replies to the model provider, which is more than voice sends today (the user's own spoken commands). It is off until turned on in Settings, which says so.
- **D5: The daemon supplies the reply (rule 18).** The final reply arrives in the daemon (hooks, session logs), and a remote deck's files are reachable only by its daemon, so the daemon passes it on through an additive, capability-gated request or event field. A client that finds no capability says reading is not available on that daemon.
- **D6: Interruptible and quiet by default.** Speech is short, any new speech for the same agent replaces what is queued rather than piling up, and "stop" or "quiet" always works.
- **D7: Must not hear itself.** The microphone does not treat the app's own speech as a command: listening pauses while speaking, or echo cancellation is used. Decided in D8.

- **D8: Echo handling is hybrid (M1, 2026-10-07; narrowed 2026-10-07).** Listening continues while the app speaks, but while speech is playing only the interrupt rows ("stop", "quiet") are honoured and every other utterance is dropped. The maintainer's answer also asked for echo cancellation on the microphone; that half is not available, because the desktop captures audio in Rust through `cpal` (`desktop/src-tauri/src/voice/capture.rs`, since WebKitGTK grants no `getUserMedia`) and `cpal` offers no echo cancellation. Platform echo cancellation (a PipeWire/PulseAudio echo-cancel source, macOS voice-processing I/O, Windows communications mode) is left out of this PRD. The worst case is the app interrupting itself, which is harmless; pausing the microphone was rejected because "stop" could then not be spoken over the speech.
- **D9: Speech source is provider first, with a picker (M1, 2026-10-07).** Settings → Voice offers **Auto** (the default), **Provider** and **System**. Auto uses the configured connection's text-to-speech when it offers one (an OpenAI-compatible connection does, Anthropic does not) and the operating system's voice otherwise.
- **D10: A summary always names the agent (M1, 2026-10-07)**, even when only one is open: "The tester finished: all 42 tests pass." It stays unambiguous if the user has switched panes.
- **D11: Reading ends with its pane (M1, 2026-10-07).** Reading is bound to the agent it was turned on for. Opening a different agent's pane, closing this one, "reading off" or "voice off" ends it, and the app says "Reading off". It never follows the user to another pane.
- **D12: Not behind the experimental flag (M1, 2026-10-07, CLAUDE.md rule 9).** Reading ships visible by default; it stays opt-in through the Settings switch (D4). No `show_*` wrapper and no `graduate-*` follow-up.

## Milestones

- [x] **M1: Decisions recorded.** (2026-10-07: D8–D12.) The open questions below answered with the maintainer and written into this document, including the CLAUDE.md rule 9 experimental-flag question for this new voice surface.
- [x] **M2: Daemon turn-end reply.** The daemon makes each finished turn's final reply available to clients for a given agent, from the agent's own data (Claude Code hooks, Codex session log, and whatever OpenCode provides), additive and capability-gated per rule 18, with rule 12 answered and the cross-version check run. (Done: `CAP_TURN_REPLIES` / `SubscribeTurnReplies`, no `PROTOCOL_VERSION` bump, no contract break; Claude Code, Codex, OpenCode and Pi. Cross-version: the PR's `cross-version` CI job.)
- [x] **M3: Summaries.** The desktop turns a final reply into one or two spoken-length sentences through the existing voice model connection, bounded in input and output, with a deterministic fallback when the model fails (e.g. "The tester finished its turn."). (Done.)
- [x] **M4: Speech.** Provider text-to-speech where available, the OS voice otherwise; interrupt and replace-queue behaviour; the microphone does not hear the app's speech. (Done; echo handling per D8 as narrowed.)
- [x] **M5: The reading mode.** `reading on` / `reading off` / `stop` rows, scoped to the open agent, the visible indicator, the Settings opt-in with its explanation, and the end conditions (pane closed, voice off). Permission prompts and errors are announced immediately. (Done; the interrupt row is `hush_reading`.)
- [x] **M6: Agent parity.** Claude Code, Codex and OpenCode covered, or the gap named per agent in the user docs (rule 20); Pi and Devin documented as covered or not. (Done: Claude Code, Codex, OpenCode and Pi covered; Devin documented as expected but not verified.)
- [x] **M7: Tests.** Unit tests for the summary prompt bounds and fallback, the mode's state machine and end conditions, and the daemon's turn-end reply for each covered agent from captured real payloads; a real-agent lane-2 test (rule 4) with an interactive Claude Code on a cheap model, asserting a turn-end summary event reaches the desktop after "reading on". (Done: unit tests, lane-1 `voice/reading-reply/001` and `/003`, lane-2 real-agent `voice/reading-reply/002` Claude, `/004` OpenCode, `/005` Codex, `/006` Pi.)
- [x] **M8: Docs.** `docs/desktop/voice.md` (what reading does, how to turn it on, privacy, per-agent coverage), developer docs for the daemon side under `docs/develop/`, and a changelog fragment (rule 19). (Done: `docs/desktop/voice.md`, `docs/develop/turn-replies.md`, `changelog.d/1497.feature.md`.)

## Risks

- **Privacy surprise.** Agent replies can contain secrets or private code. Mitigated by D4's opt-in and by sending only the final reply, bounded, never tool output or terminal content.
- **Turn-end data varies by agent and version.** The fields are the agents' own and can change. The daemon must tolerate their absence and report "not available" rather than fail silently.
- **Codex turn ends.** Reading depends on knowing when a turn ends; #1493 is fixing Codex's status accuracy, and this PRD should build on it rather than work around it.
- **Latency and cost.** One model call per finished turn, only while reading is on, bounded input. Acceptable, but measure it.
- **Speech availability on Linux.** The OS voice is not available on every Linux desktop; the provider path covers those, and the docs say what is needed.
- **Self-triggering.** The app's speech heard as a command (D7).

## Open questions

All five were answered with the maintainer on 2026-10-07 (M1):

1. Echo handling → D8 (only stop/quiet honoured while speaking; echo cancellation is not available on the `cpal` capture path).
2. Speech source order → D9 (provider first, OS voice fallback, Auto / Provider / System picker).
3. Name the agent → D10 (always).
4. Persistence across panes → D11 (ends with the pane).
5. Experimental flag → D12 (no; visible by default, opt-in in Settings).

## Success criteria

- With reading on, a user who never looks at the screen knows, within a few seconds of each turn ending, what the open agent did, and hears every permission prompt as it appears.
- No terminal output is ever read aloud, and nothing about the agent's own behaviour changes.
- Reading never starts without the Settings opt-in, and always stops on "reading off", closing the pane, or "voice off".
