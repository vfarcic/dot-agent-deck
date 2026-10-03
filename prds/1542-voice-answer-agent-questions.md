# PRD #1542: Answer the open agent's questions by voice

**Status**: Draft
**Priority**: Medium
**Created**: 2026-10-03
**Issue**: [#1542](https://github.com/vfarcic/dot-agent-deck/issues/1542)
**Related**: [#1497](https://github.com/vfarcic/dot-agent-deck/issues/1497) (hearing the agent's turns, including its permission prompts), [#1541](https://github.com/vfarcic/dot-agent-deck/issues/1541) (interrupt, clear, scratch that)

## Problem Statement

An agent stops and asks — a permission prompt ("Allow this command?"), or a menu of options. Its card shows **Needs Input** (`WaitingForInput`). With voice on, the desktop app has no command that answers it: there is no "yes", "no" or "option 2". "Type 1" may work for some agents, but only by accident, and it goes through the Commands model and the five-second send countdown like any dictation. So hands-free use ends exactly when the agent needs the user.

#1497 plans to *speak* permission prompts aloud. Without this PRD, the user hears the question and still has to reach for the keyboard to answer it.

The TUI has a partial precedent: in command mode, `y` / `n` on a card in `WaitingForInput` writes a literal `y` or `n` to the pane (`Action::SendPermissionResponse`, `src/ui.rs`). It sends the same character whatever the agent is, and nobody has recorded whether that is the right answer for each agent's current prompts.

## Solution Overview

While the open agent is asking a question the deck can recognise, voice answers it:

- **"yes" / "allow"**, **"no" / "deny"**, and where the agent offers it, **"always allow"**;
- **"option 2"**, "the second one", or the option's own words, for a menu.

Each answer is sent as the keys **that agent's** question expects. Answers are available only while a question is known to be pending, they are checked against the question still on screen when they arrive, and what was answered is shown.

**This is agent-specific by nature.** To answer, the deck must know three things per agent: that a question is pending, what its options are, and which keys pick each one. Agents report these differently, or not at all: some send it through hooks, some only draw it on the terminal. The first milestone is an inventory, and the PRD ships only what each agent can genuinely support, with the gaps documented (CLAUDE.md rule 20).

## What the user sees

- An agent asks for permission. With #1497 on, the app reads it out; either way the card and pane show **Needs Input**.
- The user says "yes". The app answers the prompt, the row beside the Voice button says *Allowed: <what was asked>*, and the agent continues.
- For a menu, the user says "option 2" or the option's words; the row names the option chosen.
- If the question changed or was answered by keyboard while the app was working out what was said, nothing is sent and the app says why.
- On an agent whose questions the deck cannot read, "yes" runs nothing and the app says that this agent's questions must be answered by keyboard.

## Scope

### In scope

- An inventory of the questions each agent in `src/agent_registry.rs` asks (permission prompts, multiple-choice questions, other confirmations), how the deck can know each is pending and what its options are, and which keys answer it — verified against current agent releases.
- A shared way to know a question is pending and what it offers, used by voice and by #1497's reading of prompts, so the two never disagree about what is being asked.
- Voice answers on the desktop app's agent screen.
- The TUI's `y` / `n` moved onto the same per-agent answers, so both clients answer the same way (CLAUDE.md rule 18: one implementation, not two).
- Tests, user docs and developer docs.

### Out of scope

- Interrupting, clearing and scratching — [#1541](https://github.com/vfarcic/dot-agent-deck/issues/1541).
- Speaking questions aloud — [#1497](https://github.com/vfarcic/dot-agent-deck/issues/1497).
- Answering a question in an agent other than the one on screen (for example from the dashboard) — the user must be able to see what they are approving. M1 may revisit this only with a design that shows the question first.
- Answering free-text questions, which are ordinary dictation.

## Design decisions and constraints

- **Approving is consequential.** "Yes" to a permission prompt lets an agent run a command or edit files. So an answer is matched on this machine from a short, fixed set of phrases rather than guessed by the Commands model, is refused when no question is known to be pending, and is refused when the question changed after the user spoke. M1 decides whether "always allow" — which removes future prompts — is offered by voice at all, or only with an on-screen confirmation.
- **Know the question from a reliable source.** Hook or plugin payloads that carry the question and its options are preferred to reading the terminal screen, which changes with agent versions and terminal width. Where an agent offers neither, it is unsupported and documented — not approximated (rule 20: "cannot" means no channel exists).
- **The deck owns it if the deck is where it is known.** Hooks and PTYs belong to the daemon. If pending-question detection lives there, it is exposed to both clients through the protocol's graded path (CLAUDE.md rule 18: an additive optional field, or a capability-gated request), with rule 12's cross-version check run.
- **Coordinate with #1497.** Both PRDs need "a question is pending, and this is it". Whichever lands first builds that; the other uses it. Neither builds its own.
- **Staleness rules unchanged.** As for every voice command: nothing is sent if the pane changed, a confirmation is open, or the agent was replaced.

## Milestones

- [ ] **M1 — Inventory and decisions.** Per agent (Claude Code, OpenCode, Codex, Pi, Devin): the kinds of questions it asks, how the deck can know each is pending and what it offers, and which keys answer it, verified against named agent versions; the result for the TUI's literal `y` / `n` on each. Then the decisions: the rule 9 experimental-flag answer; the phrases; "always allow" by voice or not; where detection lives (daemon or client) and its wire shape; the shared model with #1497.
- [ ] **M2 — Pending questions, known and exposed.** For every agent M1 marked as supported, the deck knows when a question is pending and what its options are, and both clients can read it; rule 12 answered and the cross-version check run if the wire changed.
- [ ] **M3 — Answering by voice.** Yes/no, the agent's options by number or by words, and "always allow" if M1 kept it; outcome reports and every refusal.
- [ ] **M4 — The TUI on the same answers.** `y` / `n` sends the agent's own answer instead of a literal character, so a wrong-key result found in M1 is fixed for TUI users too.
- [ ] **M5 — Tests.** Unit tests for detection and answer keys per agent; desktop tests for the voice rows and refusals; the TUI binding tests updated; a real-agent test (lane 2, CLAUDE.md rules 4 and 5) where a cheap-model agent hits a permission prompt, is answered through the same path voice uses, and continues.
- [ ] **M6 — Docs and release notes.** `docs/desktop/voice.md`, `docs/session-management.md` (the Needs Input row and per-agent coverage), `docs/develop/voice-first-design.md`, the docs-screenshots-review skill, and a changelog fragment (rule 19).

## Risks

- **A misheard "yes" approves something.** Mitigated by local matching of a small phrase set, a pending-question check and a staleness check; M1 weighs whether some prompts (for example "always allow") need on-screen confirmation.
- **Agents change their prompts.** Detection that reads the screen breaks on an agent upgrade. Mitigated by preferring hook data and recording verified versions; the lane-2 test catches a regression for the agents it covers when it is run.
- **Coverage may be thin.** Some agents may expose no reliable channel. That is an acceptable outcome if it is documented; inventing a fragile one is not.

## Open questions

1. Is "always allow" offered by voice?
2. Does pending-question detection live in the daemon, and if so, what does the wire carry?
3. Should the answer be allowed from the dashboard if the question is shown there, or only from the agent's own screen?

## Success criteria

- On every agent M1 marked as supported, a user can answer permission prompts and menus by voice without touching the keyboard.
- No answer is sent unless a question is known to be pending and still the same one.
- The TUI's `y` / `n` and voice answer a given question with the same keys.
- Every unsupported agent is named in the docs, with what it lacks.
