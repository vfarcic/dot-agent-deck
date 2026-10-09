# Turn replies: the daemon side of voice reading

The desktop's voice **reading** (PRD #1497, user docs in [`docs/desktop/voice.md`](../desktop/voice.md#reading)) speaks a short summary of each turn every agent on the deck being viewed finishes, while Settings → Voice → Reading is on. It opens one `subscribe-turn-replies` stream per agent (decision 3 of 2026-10-09), started as agents appear and dropped as they exit (`DeckReader` in `desktop/src/lib/reading.ts`). The summary is written from the agent's **final reply** for that turn, taken from the agent's own turn-end data and never from terminal output. This page is the daemon half: how a reply reaches the daemon from each agent, how the daemon hands it to a client, and what keeps it out of everything else. The desktop half (summarising, speech, the mode's state machine) lives under `desktop/src-tauri/src/voice/` (`reading.rs`, `summary.rs`, `speech.rs`) and `desktop/src/lib/reading.ts`.

## The wire: `turn-replies` and `SubscribeTurnReplies`

- **Capability** `CAP_TURN_REPLIES` (`"turn-replies"`, `src/daemon_protocol.rs`), advertised on every platform.
- **Request** `AttachRequest::SubscribeTurnReplies { id }`, where `id` is the agent's registry id. The daemon answers an OK `RESP`, then writes one `KIND_EVENT` frame per turn of that agent that ends after the subscription opened. Each frame is a JSON `TurnReply { agent_id, pane_id, sequence, reply: FinalReply { turn_id?, text, failed } }`. `sequence` increases with every reply the daemon delivers, across agents, so it is not contiguous per agent.
- **No replay.** `handle_subscribe_turn_replies` opens its broadcast receiver before it writes the OK, so every reply published after the client reads the confirmation is on the stream and nothing published before is.
- **Refusals**, with nothing opened: `id` names no agent of this daemon (`subscribe-turn-replies: no such agent`), or the daemon already serves `MAX_TURN_REPLY_SUBSCRIBERS` (32) such streams (`subscribe-turn-replies: too many subscriptions`). Since reading went deck-wide that cap is per daemon across every window reading it: on a deck with more agents than that, the agents past it are not read, and the desktop reports that start as the deck not answering. A slot is held until its connection ends (`TurnReplyReceiver` owns the semaphore permit).
- **Lag.** The hub's broadcast channel holds 64 replies; a subscriber that falls further behind has its stream ended as lagged, exactly as `subscribe-events` does (both go through `forward_broadcast`).
- **Agent exit.** Once the agent's process is gone, the daemon forwards any of its replies already received and then ends the stream with `KIND_STREAM_END` carrying `TURN_REPLIES_END_AGENT_EXITED` (`agent-exited`). The desktop treats that as the end of reading ("Reading off.") rather than waiting on a stream nothing will write to.
- **Replies never ride `BroadcastMsg`.** They travel only on this request's own connection, so a client that never subscribes never receives an agent's reply text, and no existing stream's payload changed.

**Rule 12/18 classification: additive, capability-gated, no bump.** The one sender, `DaemonClient::subscribe_turn_replies` (`src/daemon_client.rs`), withholds the request unless `CAP_TURN_REPLIES` is advertised, and reads the capability from a **fresh** `Hello` rather than the cached set, because reading is turned on at an arbitrary moment, possibly long after the cache was filled and the daemon behind the socket replaced. A daemon replaced by an older build between that `Hello` and the request refuses the unknown variant and opens nothing. No existing field changed meaning, so there is no `CONTRACT_BREAKS` entry and no `.breaking.md`. `tests/turn_replies.rs` covers the advertisement and the client's refusal without the capability.

## How a reply reaches the daemon

Every route ends in `TurnReplyHub::publish` (`src/turn_reply.rs`), held by `AgentPtyRegistry` because the hook loop, the Codex rollout monitor and the attach server all share it. `AgentPtyRegistry::publish_turn_reply` publishes only while the named agent is the pane's **live owner**, so no payload can speak for another pane's agent.

### The hook-socket line key `turn_reply`

Hook producers put the reply on the hook-socket line **beside** the event, under `TURN_REPLY_LINE_KEY` (`"turn_reply"`), never inside `AgentEvent`. That is the same choice `PresentedToken` makes for the capability token: the event the daemon keeps and broadcasts never carries the reply, so no fan-out path has to remember to strip it. An older daemon ignores the key, because `AgentEvent` does not deny unknown fields.

`publish_hook_turn_reply` (`src/daemon.rs`) publishes it only when all of these hold: the event is not daemon-synthetic and not unproven (issue #318's gate), its attested agent (when there is one) is the agent it names, it names both a pane and an agent, that agent is the pane's live owner, and the event is a turn end (`Idle`, or the `Error` / `QuotaBlocked` a failed turn becomes). It runs **before** the event is broadcast, so a client that has seen a turn end and subscribes afterwards is not handed that turn's reply as new.

`reply_from_line` reads the key leniently (a reply of the wrong shape costs the reply, never the event) and re-bounds it with `normalize`, because the socket accepts lines from any same-uid producer, not only the deck's CLI: the text is clamped to `MAX_TURN_REPLY_BYTES` (8192) at a UTF-8 boundary (`clamp_turn_reply`), a reply with no text left is dropped, and a `turn_id` longer than 256 bytes is dropped (the reply is still delivered, without de-duplication).

### Per agent

| Agent | Where the reply comes from | Failed turn |
| --- | --- | --- |
| Claude Code | The `last_assistant_message` of a main-agent `Stop` hook (`src/hook.rs`). A `Stop` fired inside a subagent (its payload names an `agent_id`) carries none. | `StopFailure`'s `last_assistant_message`, marked failed. |
| Codex | Its `Stop` hook's `last_assistant_message`, and the rollout's `task_complete` `last_agent_message` (`src/codex_rollout_tail.rs`), which carries the turn's `turn_id`. | The rollout's `task_complete` whose `error` is an object, marked failed. An errored turn runs no `Stop` hook, so the rollout is its only source. |
| OpenCode | The deck's plugin (`src/opencode_manage.rs`) attaches the session's last assistant text as `reply` to its `session.idle` report. | `reply_failed` when the turn ended on a `session.error`. An interrupted turn carries no reply. |
| Pi | The bundled extension's `agent_settled` report: `agent-event --turn-reply-stdin` with the reply written to the CLI's stdin; the CLI puts it on the line under `turn_reply`. | `--turn-reply-failed`. |
| Devin | Its hooks take the same Claude-compatible path and name the same `last_assistant_message`, so it is expected to work; it has not been checked against a real Devin. | As Claude Code. |

### Codex: the turn-id carry-over and hook/rollout de-duplication

A Codex turn is reported twice: by its `Stop` hook, whose payload names no turn, and by its rollout's `task_complete`, which does. `TurnReplyHub::publish` delivers a reply once per `(agent, turn_id)` (the `delivered` map, the oldest of `MAX_REMEMBERED_TURNS` = 1024 agents forgotten first), but only when **both** routes name the turn. So the daemon carries the turn id over: a Codex `UserPromptSubmit` arrives as a `Thinking` carrying `CODEX_TURN_ID_METADATA_KEY` (`codex_turn_id`), and `publish_hook_turn_reply` records it with `begin_turn`; the `Stop` reply that names no turn is then given it with `take_begun_turn`, which forgets it so it names one turn end only. The rollout monitor publishes `take_replies()` before it reports a failure, as the hook loop publishes before its event.

The alternatives, and why not: suppressing the `Stop` reply would leave only the rollout, which is polled every `codex_rollout_tail::POLL_INTERVAL` and only when Codex named a rollout to watch; suppressing the rollout's would lose the errored turn it alone reports; matching on the text would merge two turns that end with the same words. A reply with no turn id is always delivered, so in the worst case (the begun turn forgotten past the bound) one turn is delivered twice, never dropped.

### OpenCode: root sessions only, and the plugin's bounds

A subagent's child session shares the root's directory, streams its own assistant text interleaved with the root's turn and goes idle first, and its reply is not the user's turn (audit AU-B1). The plugin therefore attaches a reply only for a session **known** to be a root (no `parentID`) when it goes idle: ancestry is learned from `session.created` / `session.updated`, or, when no event named it, by asking OpenCode's client in the background (`lookUpAncestry`), started at the session's first assistant message so the answer is usually in by the turn's end. The idle report never waits for that answer: a session whose ancestry is still unknown is treated as not a root and its idle report carries no reply, sent at once. The reply is dropped rather than sent later, because the daemon takes a reply only on a turn-end report and a second idle report would end the turn twice; a session that neither an event nor the client ever names stays replyless on purpose (it might be a subagent's). The lookups are bounded: one per session at a time, none started while `MAX_ANCESTRY_LOOKUPS` (8) requests are unsettled, each aborted through the request's `signal` after `ANCESTRY_LOOKUP_MS` (2000 ms), and an answer that arrives after its session was deleted or OpenCode disposed is discarded.

The plugin's reply state is bounded so it cannot grow with a long session: a message keeps at most `MAX_REPLY_BYTES` (8192, the daemon's own bound, so nothing the plugin kept is cut again) of UTF-8 text across all its parts and at most `MAX_REPLY_PARTS` (256) parts, and at most `MAX_REPLY_MESSAGES` (64) messages and `MAX_REPLY_SESSIONS` (256) sessions are tracked at once, the oldest evicted first. A child session's reply state is dropped as soon as it is known to be a child.

### Pi: stdin transport

The reply never goes on a command line, where any local user who can read `/proc/<pid>/cmdline` could read it. `pi.exec` spawns with stdin ignored and takes no stdin option (checked on Pi 0.84.4 and 1.1.0), so the extension spawns the one report that carries a reply itself (`execWithStdin` in `orchestrator.ts`, called with `node:child_process`'s `spawn` from `index.ts`): the same binary, an argv array with no shell, the session's directory, killed on the report's abort signal, with the reply (cut to `MAX_TURN_REPLY_BYTES` of UTF-8) written to stdin and stdin closed. Every other report still goes through `pi.exec`. The CLI's `--turn-reply-stdin` reads stdin only on a `--type finished` report, at most `MAX_TURN_REPLY_BYTES` plus three bytes (so a character straddling the bound is complete before `normalize` cuts it), and gives up after `TURN_REPLY_STDIN_TIMEOUT` (2 s) on a stdin nobody closes, sending the report without a reply (`read_turn_reply_stdin` in `src/hook.rs`). The older-CLI ladder (an older CLI refusing the reply flags drops the reply for the session and keeps the report's level) and the reply tracker are in [`pi-extension.md`](pi-extension.md).

## Keeping the reply out of the log

A turn-ending hook line carries the agent's private reply, and several hook-loop diagnostics log the raw line (an unrecognised `event_type`, a `Malformed event:` line, one that parses but is not an event). `redact_hook_token` (`src/daemon.rs`) withholds it on both of its paths, next to the capability-token masking:

- a line that parses: every `turn_reply` member, at any depth, is replaced by `<withheld: N bytes>` (`withhold_turn_replies`);
- a line that does not: nothing from the first occurrence of `turn_reply` onwards is logged, only `<withheld: a turn reply and the N bytes from it on>`. Every producer serializes the key after the event's identifiers (`serde_json` writes members in sorted order), so what is logged is still the part a diagnostic needs.

`hook_diagnostics_never_log_a_turn_reply` drives the real hook loop with lines built by the production producer (`agent_event_cli_line`) through all three diagnostics and asserts a sentinel reply never reaches the captured log while each warning still names the line.

## What the desktop does with it

`voice::reading::DaemonTurnEvents` opens two connections per reading session: this reply stream for the agent, and the ordinary `subscribe-events` status stream, from which it takes the agent's permission prompts (`PermissionRequest`, or a `WaitingForInput` that is a permission prompt), turn-ending errors and `QuotaBlocked`. `coalesce` merges them so a failed turn reported by both a reply and a status is announced once. A finished turn is spoken only from a reply, so a turn that ends with no reply is not announced. The reply stream ending (the agent exited) ends the session.
