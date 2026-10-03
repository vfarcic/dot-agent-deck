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

## M1 inventory (verified 2026-10-03)

This section is M1's research. **Nothing in it is a decision**: the recommendations at the end are recommendations pending the user's answers to the open questions, and no milestone checkbox is ticked by it.

### Versions and how each fact was established

| Agent (`src/agent_registry.rs` `ALL`) | Version checked | How |
| --- | --- | --- |
| Claude Code (`CLAUDE_CODE`, native hooks) | `2.1.289` | Driven interactively on Haiku 4.5 in a private tmux server, in a scratch project, with user settings excluded (`--setting-sources project,local`) and a `--settings` file whose hooks appended each raw hook payload to a log. Prompts captured with `tmux capture-pane`, keys sent with `tmux send-keys`. Hook docs at `code.claude.com/docs/en/hooks` read for the decision schema. |
| Codex (`CODEX`, wrapper + native hooks) | `codex-cli 0.160.0` | Driven interactively on `gpt-5.6-luna` (low effort) with `-s read-only -a on-request` so a write asks. The deck's own already-installed and trusted hooks were pointed (`DOT_AGENT_DECK_SOCKET`) at a scratch listener, so what was recorded is the `AgentEvent` **the deck builds today**, not Codex's raw payload. Hook docs at `learn.chatgpt.com/docs/hooks` read. Open PR #1523's diff read. |
| OpenCode (`OPEN_CODE`, plugin) | `1.18.34` (SDK/plugin types read from the installed `@opencode-ai/sdk` / `@opencode-ai/plugin` `1.18.32`) | Driven interactively on `openai/gpt-5.6-luna` with `OPENCODE_CONFIG_CONTENT` setting `bash` and `edit` to `ask`, the deck's installed plugin pointed at the same scratch listener. Event and reply-endpoint shapes read from `types.gen.d.ts`. |
| Pi (`PI`, extension) | `0.87.1` | Docs shipped in the installed package (`docs/security.md`, `docs/extensions.md`, `docs/rpc-extension-ui.md`, `docs/how-pi-works.md`) and the deck's own extension (`pi-extension/src/orchestrator.ts`). Not run. |
| Devin (`DEVIN`, native hooks) | `devin 3000.11.3` | **Not run**: `devin models list` answers `Not logged in` on this machine. Docs at `docs.devin.ai/cli/extensibility/hooks/overview` and `docs.devin.ai/cli/reference/permissions`, `strings` on the binary, and the deck's `src/devin_hooks_manage.rs`. |

Every cell below is tagged: **[observed]** seen on the named version; **[docs]** read from the agent's documentation or shipped types; **[code]** read from this repository; **[inferred]** reasoned, not seen; **[unverified]** not established either way. Rule 17 applies: an untagged absolute below is a defect in this section.

### What the deck receives today

- **Claude Code.** The deck installs `Notification` with matcher `permission_prompt` and **not** `PermissionRequest` (`HOOK_TYPES` and `make_rule`, `src/hooks_manage.rs`) [code]. So a Claude card turns Needs Input from a `Notification` whose payload is `{"message":"Claude needs your permission","notification_type":"permission_prompt"}` — no tool, no question, no options [observed]. The tool is known only from the preceding `PreToolUse` (`tool_name`, `tool_detail`).
- **Codex.** `PermissionRequest` is installed (`CODEX_HOOK_EVENTS`) [code]; the deck's event is `permission_request` with `tool_name: "Bash"`, `tool_detail: "touch codex_m1.txt"` and empty metadata [observed]. Its `request_user_input` question tool arrives only as `tool_start` with `tool_name: "request_user_input"` and `tool_detail: null`, so the card reads **Working**, not Needs Input, while that menu is on screen [observed].
- **OpenCode.** The plugin forwards `permission.asked` / `permission.replied` (`src/opencode_manage.rs`) [code], but its `permissionPayload` looks for `prompt`/`title`/`message`/`text`/`question`, none of which `permission.asked` carries in 1.18 [docs: types], so the deck's event arrives with `user_prompt: ""` and no tool [observed]. The `question` tool arrives as `tool_start` with `tool_name: "question"` and no detail; `question.asked` is not forwarded at all, so the card reads **Working** while that menu is on screen [observed].
- **Pi.** The deck's extension never reports `waiting` (`piEventToAgentState`, `pi-extension/src/orchestrator.ts`) [code].
- **Devin.** `PermissionRequest` is installed (`DEVIN_HOOK_EVENTS`) and maps to Needs Input [code]; payload contents not observed [unverified].
- **None of the options reach the deck on any agent.** `AgentEvent` (`src/event.rs`) has no field for a question or its options, `tool_input` is reduced to a truncated `tool_detail`, and `SessionSnapshot` (`src/state.rs`) carries only `status` [code].

### Per-agent inventory

#### Claude Code 2.1.289

| Question | Pending + options from | Keys that answer | Answer without keys | TUI literal `y` / `n` |
| --- | --- | --- | --- | --- |
| Tool permission (Bash, Edit…) — menu `1. Yes` / `2. Yes, and always allow access to <dir> from this project` / `3. No`, footer `Esc to cancel · Tab to amend` [observed] | `PermissionRequest` hook: `tool_name`, `tool_input`, `permission_suggestions` (e.g. `{"type":"addDirectories",…}`, `{"type":"setMode","mode":"acceptEdits"}`), `permission_mode`; **no `tool_use_id`** on it (the preceding `PreToolUse` has one) [observed]. The menu's labels and count are **not** in the payload; they are drawn on screen only [observed]. `Notification` `permission_prompt` also fires, observational only [observed; docs]. | A digit selects **and submits** with no Enter: `1` ran the tool, `3` denied [observed]. `Esc` cancels per the footer [observed footer; deny effect unverified]. | Yes: a `PermissionRequest` hook returning `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` (or `"deny"`, with `message`) [docs]. **The dialog is shown while the hook is still running**, a late `allow` returned 5 s in dismissed the visible dialog and ran the tool ("Allowed by PermissionRequest hook"), and when the keyboard answered first a later `deny` from the hook was ignored [observed]. `updatedPermissions` for "always" [docs, untested]. | **Ignored**: neither key changed the menu [observed]. The TUI still reports "Permission approved." / "Permission denied." — a false report. |
| Multiple choice (`AskUserQuestion` tool) — `1. Red` … `3. Blue`, `4. Type something.`, `5. Chat about this` [observed] | `PreToolUse` **and** `PermissionRequest` with `tool_name: "AskUserQuestion"`, `tool_input.questions[]` = `{question, header, options[{label, description}], multiSelect}` [observed]. `Notification` `permission_prompt` also fires, so the card already reads Needs Input [observed]. | A digit selects and submits a single-question, single-select menu (`2` → "Green") [observed]. Multi-question and `multiSelect` forms [unverified]. | Yes: `PermissionRequest` `allow` with `updatedInput` = the `tool_input` plus `answers: {"<question>": "<label>"}` answered it ("→ Blue", "Allowed by PermissionRequest hook") [observed]. | **Ignored** [observed]. |
| Plan approval (`ExitPlanMode`) — `1. Yes, auto-accept edits` / `2. Yes, manually approve edits` / `3. Tell Claude what to change` [observed] | `PermissionRequest` with `tool_name: "ExitPlanMode"`, `tool_input.plan`, `tool_input.planFilePath`; `Notification` message "Claude Code needs your approval for the plan" [observed]. Option labels not in the payload [observed]. | Digits [inferred from the two menus above; not pressed here]. | `PermissionRequest` `allow` / `deny` [docs; untested for this tool]. Which of options 1/2 an `allow` maps to [unverified]. | **Ignored** [observed]. |
| Folder trust at startup — `❯ No, exit` / `Yes, I trust this folder` (no numbers, `No` preselected in a fresh folder) [observed] | No hook fires: the session has not started [observed: the log was empty]. | Arrows + Enter [observed]; `y` and `2` did nothing [observed]. | No. | n/a — no Needs Input is raised. |
| MCP elicitation forms | `Elicitation` hook with `form_schema` [docs]; `Notification` types `elicitation_dialog` / `elicitation_url_dialog` exist [docs] but the deck's matcher passes only `permission_prompt` [code]. | [unverified] | `Elicitation` output `action: accept/decline/cancel` + `content` [docs]. | n/a today. |

Two further Claude facts that matter to the design. **A keyboard deny leaves the card on Needs Input**: after `3`, no `PostToolUse`, `Stop` or other hook fired (the log ended at `Notification`) [observed], so the deck believes the question is still pending until the next prompt. And **adding `PermissionRequest` to the installed hook set needs a version gate measured first**: issue #714 measured that an older Claude Code given one unknown hook key drops *every* hook in `settings.json` (`STOP_FAILURE_MIN_CLAUDE_VERSION`), and the first release that accepts `PermissionRequest` is [unverified].

#### Codex 0.160.0

| Question | Pending + options from | Keys that answer | Answer without keys | TUI literal `y` / `n` |
| --- | --- | --- | --- | --- |
| Command approval — `1. Yes, proceed (y)` / `2. Yes, and don't ask again for commands that start with <prefix> (p)` / `3. No, and tell Codex what to do differently (esc)`, footer `Press enter to confirm or esc to cancel` [observed] | `PermissionRequest` hook: `turn_id`, `tool_name`, `tool_input` (with `description` as the reason) [docs]; the deck records `tool_name` + `tool_detail` [observed]. Option labels not in the payload [inferred: the docs list none]. | `y` approved [observed]. `n` — not shown on screen — denied and interrupted the turn ("You canceled the request…", "Conversation interrupted") [observed]. `p` and `esc` per the labels [observed labels; effect untested]. Digits on this menu [unverified]. | `PermissionRequest` returning `decision.behavior` `allow` / `deny`; "an allow proceeds without surfacing the approval prompt"; `updatedInput`, `updatedPermissions` and `interrupt` "fail closed today" [docs]. **Whether the prompt is visible while a hook is still deciding is [unverified]** — the probe that would measure it needed `--dangerously-bypass-hook-trust` and was not run; the docs' wording suggests the hook runs *before* the prompt is shown [inferred]. | **`y` correct** (approves) and **`n` correct** (denies) [observed]. A `y` sent after the prompt is gone is typed into the composer, unsent [observed]. |
| Multiple choice (`request_user_input`, Plan mode) — `1. Red` … `3. Blue`, `4. None of the above`, footer `tab to add notes · enter to submit answer · esc to interrupt` [observed] | `PreToolUse` only, `tool_name: "request_user_input"`; no `PermissionRequest`, so the card reads Working [observed]. Whether the raw `PreToolUse` `tool_input` carries the questions and options [unverified]: the deck's builder keeps no `tool_input`. | A digit selects and submits (`2` → "answer: Green") [observed]. | None documented [docs: the hook page does not mention it]. | n/a today (card is Working); `y` on this menu is **ignored** [observed]. |
| Plan "implement?" prompt | [unverified] — the model printed `<proposed_plan>` text and no prompt appeared in the probe [observed]. | [unverified] | [unverified] | [unverified] |
| Folder trust at startup — `1. Trust and continue` / `2. Quit` [observed] | No hook [inferred: before `SessionStart`]. | Enter [observed]; `y` ignored [observed]. | No. | n/a. |

Also: after a keyboard deny no hook fired (the deck's next event was the next prompt's `thinking`) [observed], so the card stays Needs Input exactly as on Claude. Codex documents an `Interrupt` hook event and a `SessionEnd` that the deck does not install (`CODEX_HOOK_EVENTS`) [docs; code] — `Interrupt` may be the signal that clears a denied prompt [inferred, unverified]. PR #1523 (open) confirms the dependency: without trusted hooks a Codex card "shows no tool, no prompt and no **Needs Input**" — so every Codex answer path requires trusted hooks.

#### OpenCode 1.18.34

| Question | Pending + options from | Keys that answer | Answer without keys | TUI literal `y` / `n` |
| --- | --- | --- | --- | --- |
| Tool permission — `△ Permission required`, `# Shell command`, `$ touch …`, buttons `Allow once` / `Allow always` / `Reject`, footer `⇆ select · enter confirm` [observed] | Plugin bus `permission.asked`: `{id, sessionID, permission, patterns[], metadata, always[], tool?: {messageID, callID}}`; `permission.replied`: `{sessionID, requestID, reply: "once"\|"always"\|"reject"}` [docs: types]. The three replies are a fixed set [docs: types], so the options are known without reading the screen. The deck forwards the event but none of these fields [observed]. | Enter = Allow once (preselected) [observed]; Esc = Reject [observed]; arrows move between the three [observed footer; untested]. | `POST /permission/{requestID}/reply` `{reply, message?}` [docs: types]; the plugin's `ctx.client` is an SDK client [docs: types]. Whether a reply through it dismisses the TUI dialog [unverified]. | **Ignored**, both keys [observed]. OpenCode does report the reply (`permission.replied` → Thinking) [observed], so unlike Claude and Codex a keyboard answer clears Needs Input. |
| Multiple choice (`question` tool) — `1. Red` … `3. Blue`, `4. Type your own answer`, footer `↑↓ select · enter submit · esc dismiss` [observed] | Plugin bus `question.asked`: `{id, sessionID, questions: [{question, header, options[{label, description}], multiple?, custom?}], tool?}`; `question.replied` `{requestID, answers}`; `question.rejected` [docs: types]. Not forwarded by the deck's plugin, so the card reads Working [observed]. | A digit selects and submits (`3` → "Blue") [observed]. | `POST /question/{requestID}/reply` / `…/reject` [docs: types; untested]. | n/a today; `y` ignored on this menu [observed]. |

#### Pi 0.87.1

Pi's docs say it "does not ask for approval before every tool call" (`docs/security.md`), and none of its shipped docs describes a built-in tool-permission prompt [docs]; whether some built-in confirmation exists elsewhere in its TUI is [unverified]. The questions a Pi session can show are a third-party extension's `ctx.ui.confirm` / `ctx.ui.select` dialogs (`docs/extensions.md`) — which surface as `extension_ui_request` events only in **RPC mode**, not to another extension in the interactive TUI [docs; inferred for the TUI case] — and the startup project-trust prompt, which user-level extensions can answer through the `project_trust` event (`docs/security.md`) [docs]. The deck's extension reports no `waiting` state [code], so the TUI's `y` / `n` never fires on a Pi card. **Unsupported: no channel exists in the interactive TUI for an extension's dialog** [inferred from the docs, unverified by running]. The project-trust event is a channel, but for a startup question this PRD does not cover.

#### Devin 3000.11.3

| Question | Pending + options from | Keys | Answer without keys | TUI literal `y` / `n` |
| --- | --- | --- | --- | --- |
| Tool permission — options "Allow once", "Allow for session", "Allow for project", "Allow for project (local)", "Allow globally", plus "Edit command" and "Describe change to command"; MCP tools have their own four [docs] | `PermissionRequest` hook (installed, → Needs Input) [code]; payload fields beyond the common `tool_name` / `tool_input` [unverified]. | [unverified] — not logged in. | Hook output `"decision": "approve"` / `"block"` with a `reason` [docs; untested]; whether the prompt shows while the hook runs [unverified]. | [unverified]. |
| Multiple choice (`ask_user_question`) | The binary contains `ask_user_question` / `AskUserQuestionInput` strings [observed: `strings`]; whether any hook fires for it [unverified]. | [unverified] | [unverified] | [unverified] |

Devin has a channel (a decision-capable `PermissionRequest` hook), so it is **not** a rule-20 "cannot"; it is unverified until someone with a logged-in Devin measures it.

### Summary

| Agent | Permission prompt | Multiple choice | TUI `y` / `n` today |
| --- | --- | --- | --- |
| Claude Code 2.1.289 | Supportable — `PermissionRequest` hook (not installed today); answer by hook decision [observed] or digit [observed] | Supportable — `AskUserQuestion` options in the hook payload; answer by hook `updatedInput.answers` [observed] or digit [observed] | Wrong keys: ignored, and the status line claims it worked |
| Codex 0.160.0 | Supportable — `PermissionRequest` hook (installed); answer by `y` / `n` [observed]; hook decision [docs] | Not today — `request_user_input` raises no Needs Input and its options are not known to be in any payload | Correct |
| OpenCode 1.18.34 | Supportable — `permission.asked` (forwarded, fields dropped); answer by Enter / Esc [observed] or reply API [docs] | Supportable — `question.asked` (not forwarded) carries the options; answer by digit [observed] or reply API [docs] | Wrong keys: ignored, and the status line claims it worked |
| Pi 0.87.1 | None documented | Unsupported — no channel for an extension's dialog in the TUI | Never fires |
| Devin 3000.11.3 | Channel exists, everything else unverified | Unverified | Unverified |

### Recommendations (pending the user's answers — nothing here is decided)

**Coverage per agent and question kind (rule 20).**

- Claude Code: permission prompts and single-question single-select `AskUserQuestion` — supported. Plan approval — supported as yes/no only if `allow` is measured to map to a defined option; otherwise refused by voice. Multi-question / `multiSelect` forms and MCP elicitations — unsupported in the first ship, documented as "answer by keyboard" (a channel exists, so this is a scope choice, not a rule-20 gap; say so in the docs).
- Codex: command approvals — supported. `request_user_input` — unsupported until the raw `PreToolUse` payload is measured to carry the options; if it does, it becomes supportable without a new hook.
- OpenCode: permission prompts and single-question `question` menus — supported, after the plugin forwards `permission.asked`'s fields and `question.*`.
- Pi: unsupported — documented gap: "Pi documents no tool-permission prompt, and a dialog from another Pi extension cannot be read by the deck."
- Devin: unsupported **until measured**, documented as unverified rather than impossible. M2 should include the measurement on a logged-in host.

**Where detection lives: the daemon.** The hooks, the plugin bus and the PTYs are all the daemon's, and the TUI and the desktop must answer identically (rule 18). Proposed shape, following the graded path:

- `AgentEvent` (`src/event.rs`) gains an additive optional `question` field (or a JSON string under one `metadata` key, which moves no wire at all), filled by `src/hook.rs` from `PermissionRequest` / `PreToolUse` (`AskUserQuestion`) for Claude, Codex and Devin, and from the plugin's forwarded `permission.asked` / `question.asked` for OpenCode (`src/opencode_manage.rs`).
- `SessionSnapshot` (`src/state.rs`) gains `pending_question: Option<PendingQuestion>`, `#[serde(default, skip_serializing_if = "Option::is_none")]` — no `PROTOCOL_VERSION` bump (`src/daemon_protocol.rs:11-14`). Set in `apply_event` on the question event; cleared on any event that proves the agent moved on (`ToolEnd`, `UserPromptSubmit`/`Thinking`, `Idle`, OpenCode's `permission.replied` / `question.replied`, and the deck's own answer). The keyboard-deny gap on Claude and Codex means "still pending" can be wrong after a keyboard answer; the staleness check below and, on Codex, the documented `Interrupt` hook are how that is contained.
- `PendingQuestion` = `{ id, kind: permission | choice | plan, summary, options: [{ index, label, description?, role: allow_once | allow_always | deny | choice | free_text }], multi: bool, raised_at_ms }`, where `id` is the agent's own (`tool_use_id` from Claude's `PreToolUse`, OpenCode's request id) or a deck-minted one. **Every text field is untrusted model output** (`docs/develop/voice-first-design.md` §6) and #1497 will speak it.
- Answering: a new `AttachRequest::AnswerQuestion { agent, question_id, choice }` that every client withholds until the daemon advertises `CAP_ANSWER_QUESTION` in `DAEMON_CAPABILITIES`, with the check in the client library as `focus_gained_while` does — again no bump. The daemon refuses unless `question_id` is the one pending now, then answers through the agent's own channel. The per-agent mapping belongs on `AgentSpec` (`src/agent_registry.rs`, "the single place per-agent data lives"), e.g. an `answer` handler beside `hook_install`.
- Answer channel per agent: **Claude — the `PermissionRequest` hook decision** (observed working, observed safe against a keyboard answer that wins the race, needs no option-to-key mapping, carries `AskUserQuestion` answers by label and makes a stale "yes" a no-op instead of a typed `1`). Its cost: the deck's hook process must hold a connection to the daemon until an answer or a timeout, one per open prompt, and the hook key needs the version gate above. **Codex — keys (`y` / `n`)** until it is measured whether a pending hook hides the prompt. **OpenCode — Enter / Esc** for permissions and digits for questions, moving to the plugin's reply API once a reply through it is measured to dismiss the TUI dialog.
- Files that would change: `src/event.rs`, `src/hook.rs`, `src/hooks_manage.rs`, `src/opencode_manage.rs`, `src/state.rs`, `src/daemon_protocol.rs`, `src/daemon.rs` / `src/daemon_attach.rs`, `src/agent_registry.rs`, `src/ui.rs` (`Action::SendPermissionResponse`), `desktop/src-tauri/src/daemon_bridge.rs`, `desktop/src-tauri/src/voice/outcome.rs` (`local_intercept`), `desktop/src-tauri/src/voice/choice.rs` (ordinal and label matching), `desktop/src/components/VoiceControlPanel.tsx` (`VoicePane`, the one `VoiceContext` gate), `desktop/src/types.ts`. Rule 12's cross-version check applies to the new variant even though it moves no number.

**The shared model with #1497.** `PendingQuestion` on the snapshot *is* the model: #1497 reads `summary` and `options` to speak a prompt, this PRD answers by `id`. Neither keeps its own copy, and #1497 should not speak a prompt this snapshot does not carry, so the two cannot disagree about what is being asked.

**Phrase set, matched locally and whole-utterance** (the `voice/dictation.rs` reserved-set rule, live only while the pane on screen has a `pending_question`):

- allow: "yes", "allow", "approve", "yes allow", "allow it"
- deny: "no", "deny", "reject", "don't allow"
- always allow: "always allow" (see below)
- choice: "option N" / "number N" for N in 1..9, ordinals ("the first one", "the second one"), and an option's label as the whole utterance, through `voice::choice::answer`'s rules. A bare number is not offered, to keep dictated numbers dictatable.
- Options with `role: free_text` ("Type something.", "Chat about this", "None of the above", "Tell Claude what to change") are refused by voice with "answer that one by keyboard".

**"Always allow" by voice** — pros: it is the answer a hands-free user most wants on a repetitive prompt, and every supported agent offers one. Cons: it removes future prompts, its scope differs per agent and per prompt (Claude: a directory or rule for this project; Codex: a command prefix; OpenCode: the request's `always` patterns; Devin: five scopes), a mishearing is not undone by the next utterance, and on Codex the hook decision cannot express it at all (`updatedPermissions` fails closed). **Recommendation: offer it only through an on-screen confirmation that names its scope** (the D5 `ConfirmDialog` pattern), and not on Codex until its key `p` is measured. Not offering it at all is the safer fallback if the confirmation is judged too heavy for the first ship.

**Experimental flag (rule 9).** Recommendation: **yes**, behind a `show_voice_answers()` wrapper gated at the voice row and at the TUI's new behaviour. Reasons: the first ship covers three of five agents, Devin is unverified, Claude's path adds a hook key that needs a version gate, and an approval is consequential — this is a surface that should earn its default. Counter-argument worth weighing: the TUI half fixes a present bug (a false "Permission approved." on Claude and OpenCode), and a fix should not hide behind a flag; if that weighs more, gate only the voice phrases and ship the TUI change unflagged.

**Q3 — dashboard answering.** Recommendation: **the agent's own screen only** for this PRD, as scoped. The dashboard shows a status, not the question, and `pending_question.summary` is untrusted text that would have to be rendered in full, with the options, before "yes" could be safe there. The snapshot field makes a later dashboard design possible without new plumbing.

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
