# Agent-Driven Notifications — the Orchestrator Recipe and the Retired ntfy Dogfood

> **Retired 2026-07-28, and partly un-retired 2026-09-11.** This page used to document a config-only notification dogfood on *this repo's own* orchestration: a `scripts/notify.sh` helper that `curl`-POSTed to a public [ntfy](https://ntfy.sh) topic, called from per-role notify instructions in every role's `prompt_template`. The ntfy topic is still abandoned and the per-worker `blocked` pings are still gone — but `scripts/notify.sh` itself is **back**, now POSTing to Telegram, after the MCP server that displaced it turned out to cost a CPU core. See [The script came back](#the-script-came-back-2026-09-11) below.

## What replaced it

[PRD #126](../../prds/done/126-agent-driven-notifications.md) was rescoped from "dogfood, no deck code" into a shipped feature plus a much smaller recipe. Two things came out of it:

- **A daemon-side idle-worker detector** (deck code) — the daemon tracks each outstanding delegation and, after `worker_response_timeout_minutes` (default 120, `0` disables) with no `work-done`, injects one self-describing prompt into the orchestrator. The daemon never notifies; it only reports the condition, and the orchestrator decides what to do.
- **An orchestrator-only notification recipe** — the `orchestrator` role's `prompt_template` in `.dot-agent-deck.toml` sends a short, fire-and-forget Telegram message at the workflow's pause-for-human moments, including the daemon's idle-worker event. Workers never notify and never wait on the user: a blocked worker returns its question through `work-done` and the orchestrator escalates. (This originally went through a `telegram` MCP server declared in `.mcp.json`; since 2026-09-11 it goes through `scripts/notify.sh` instead, for the reason below.)

The user-facing documentation for the feature lives in the published [Idle Workers & Notifications](../idle-workers-and-notifications.md) page; the full recipe, with placeholders, is [below](#the-orchestrator-notification-recipe). The orchestrator keeps a minimal expectation log at `.dot-agent-deck/notify-log.md` (gitignored) so that "reached a notify moment but never sent" stays detectable after a compaction drops the instruction (PRD #82).

## Why the history is worth keeping

The dogfood is what produced the design, so its findings are recorded in the PRD's [Background](../../prds/done/126-agent-driven-notifications.md#background-the-dogfood-that-led-here) section rather than repeated here. In short: agent-driven notification works and is genuinely fire-and-forget; "one `.mcp.json` for every agent" is a myth (only Claude reads it natively), which is exactly why orchestrator-only is the right topology; the public ntfy topic was acceptable only because the payload was one status sentence; and the one thing config provably could not do — notice a delegated worker that went silent — is what became the daemon feature.

The full retired setup (script internals, the ntfy topic caveat, the two-record expectation log, the reconciliation procedure) is in this file's git history if you ever need it: `git log --follow -- docs/develop/notifications-dogfood.md`.

## The script came back (2026-09-11)

[Issue #1015](https://github.com/vfarcic/dot-agent-deck/issues/1015) put the `curl` helper back and deleted the `telegram` MCP server from `.mcp.json`. The trigger was operational rather than architectural: an orphaned `telegram-mcp-bot` process was found burning **~104% of one core**, and had been for **1 day 12 hours** — about 78% of all user CPU on the dev box, while every agent in the deck sat idle.

**The mechanism.** `telegram-mcp-bot@1.1.0` installs a global `uncaughtException` handler that logs instead of exiting, deliberately: *"Logging instead of dying keeps the stdio transport alive so the host stays connected."* With `startPolling()` holding the event loop open and no stdin-EOF handler anywhere, the process cannot die when its parent does. Orphaned to `PPid 1` by an unclean session exit, both its pipes break, and the handler logs the resulting `EPIPE` *through the pipe that just died* — which throws `EPIPE`, which re-enters the handler. Reproduced in isolation with the same pattern and no Telegram involved: with the polling loop it survives its parent and spins a full core; **without the polling loop it exits on its own**. The polling loop is the sole reason it is immortal.

**Why we were paying for it.** The contract above already said the orchestrator is the only agent that notifies, and already said never to read `get_updates` — an unauthenticated inbound channel and therefore a prompt-injection path. So this repo used exactly one tool, `send_message`. Long-polling `getUpdates` is the only reason that process persists after startup, and it bought us nothing. It also cost at our concurrency: Telegram permits exactly one `getUpdates` poller per bot token, and `.mcp.json` was then tracked at the repo root, so every dispatch worktree inherited it — measured at **14 processes, 924 MB RSS, one shared token**, so 13 were permanently losing a 409 fight and retrying every 30s forever. (That file has since been deleted outright, issue #1249: the two servers left in it were unused here, and a tracked MCP declaration reaches every contributor's worktrees rather than staying one developer's setting.)

**What the swap buys beyond the CPU.** Two things that were *instructions an agent had to follow* became *properties of the tool*. `chat_id` is now always explicit, because the script requires it and skips the send when `TELEGRAM_CHAT_ID` is unset — where the MCP send tools fell back to the most recently active chat, so anyone who messaged the bot first received the next notification. And there is no inbound path at all, so `get_updates` cannot be reached by an agent that ignores the rule. The per-client tool-name trap (`telegram_send_message` through `pi-mcp-adapter` versus an unprefixed `send_message` natively, which a live send once failed on) goes away too, which is the same reason the original dogfood chose a shell CLI: *"this repo runs four different agents; a shell CLI works for all of them with no per-agent wiring."*

**What this is not.** The [example recipe](#the-orchestrator-notification-recipe) below still shows the MCP wiring, and stays valid — it is explicitly framed as one swappable channel, and the 409 half of this only bites at many-agent concurrency. The orphan half does not, which is why the recipe's security requirements carry the orphaned-server caveat ([issue #1016](https://github.com/vfarcic/dot-agent-deck/issues/1016)). Nothing in the deck changed; this is repo infrastructure.

The other half of #1015 is [the orphan reaper](orphan-reaper.md), a machine-level net for the same class of leak.

## The orchestrator notification recipe

This section was Part 2 of the published [Idle Workers & Notifications](../idle-workers-and-notifications.md) page until issue #544 moved it here: it is one project's setup rather than a feature of the deck, so the user page now keeps only a short summary of its advice. It still describes the Telegram MCP wiring; this repo itself has since moved to `scripts/notify.sh` ([above](#the-script-came-back-2026-09-11)), but the design points and security requirements apply to either channel.

> **This part is an example, not a shipped feature.** Nothing here is built into the deck — it is prompt text and MCP configuration in one project's repository, reproduced because it works for us. The idle-worker detector is tested and behaves as documented; this recipe has not yet been exercised across enough real runs to make promises about, and the [compaction caveat](#what-survives-a-compaction-and-what-does-not) below is a known weakness rather than a solved problem. Treat it as a starting point to adapt, and expect to tune the moments to your own workflow.

### The channel is your choice

The deck has no opinion about where messages go. Telegram is used below because it was convenient, not because it is recommended: a Slack MCP server, an [ntfy](https://ntfy.sh) topic over `curl`, a desktop notifier, an SMS gateway, or a webhook into whatever you already watch will all do the same job. Everything in this recipe except the specific server name and tool name applies unchanged.

What matters is that the agent can reach the channel with one tool call, that the call is cheap, and that failing to send never becomes the agent's problem.

### Only the orchestrator notifies

**The orchestrator is the only agent that sends messages, and the only agent that ever waits for you.** Workers notify nobody. A worker that is blocked or needs a decision does not ping you and does not sit waiting for a reply — it returns the question through `work-done`, and the orchestrator turns it into one notification and pauses.

That is not tidiness, it is topology. Suppose a worker did message you and wait for an answer. At that same moment the orchestrator is parked waiting for that worker's `work-done`, so the run has two agents waiting and nobody working. Worse, your reply has nowhere to go: you would be replying in a chat app, while the worker is waiting on its own pane's input — the deck routes tasks to workers *from the orchestrator*, so there is no path from your phone into a worker's session. Routing every human interaction through the orchestrator keeps exactly one agent waiting on you, and it is the one that can actually act on your answer.

There is a practical bonus. Because only the orchestrator sends, only the orchestrator needs the channel wired up — which matters more than it sounds, as the next section explains.

### The four moments

Notify when the run stops needing a computer and starts needing you. In this project's workflow that is exactly four moments, and per-step chatter is deliberately absent — a message for every delegation would train you to ignore all of them.

| Moment | Example message |
|---|---|
| **Escalation** — a worker returned a question the orchestrator cannot answer alone | `myrepo PRD #126 — needs input: which timeout default?` |
| **Merge gate** — checks are green and the run stops for a merge go-ahead | `myrepo PRD #126 — needs go-ahead: merge PR #223` |
| **Run finished** — *fully* done: merged and closed, or abandoned | `myrepo PRD #126 — DONE: merged & closed` |
| **Idle worker** — the daemon's idle-worker report arrived | `myrepo PRD #126 — STUCK: coder silent >120 min` |

### Which pauses earn a message

The obvious rule is "notify at every pause for a human", and it is the wrong one. The criterion that holds up is **every pause where the human may have walked away.** A gate that fires seconds into a run, while you are still sitting there watching it start, earns nothing — you will have answered it before your phone finishes buzzing, and the message only teaches you that this channel carries things you do not need. A gate that arrives after a long unattended stretch earns a lot, because it is the only thing standing between "waiting on you" and "waiting on you for three hours".

The honest cost of that removal: if you *do* walk away immediately after starting a run, a plan waiting for approval now has no out-of-band signal at all — and idle-worker detection cannot cover the gap either, because nothing has been delegated yet at that point, so there is no outstanding delegation to time out. The run simply sits at the start until you come back to the terminal. Nothing is lost, but nothing tells you.

"Fully done" is worth spelling out in your prompt, because agents are enthusiastic about progress. A message when the PR opens, when CI goes green, when a review posts — each is a moment the agent feels is significant and you cannot act on. One message when the whole thing is over.

### Message shape

Every message starts with **repo + task identifier**, because you will eventually run several orchestrations in parallel and a message that says only `needs approval` tells you nothing about where to go. After the prefix, one clause: whether it is *done* or *needs attention*, and what. If a message needs a second sentence, the run needed a different design.

Messages of this shape survive being read on a lock screen, which is the whole point.

### Fire and forget

**Send and continue.** Never wait for an acknowledgment, never poll for delivery, never retry, and never let the result of a send change what the agent does next. A failed send is a lost notification, not a workflow event — the run carries on exactly as it would have, and you find out at the terminal instead of on your phone.

The inverse is a trap that looks reasonable: an agent that verifies delivery, or retries a failing channel, has made your chat provider a dependency of your build. Notifications are an out-of-band convenience layered on a workflow that must remain correct without them.

### Wiring the MCP server

Say the channel is a Telegram bot exposed through an MCP server. For a client that reads `.mcp.json` natively — Claude Code does — the declaration is:

```json
{
  "mcpServers": {
    "telegram": {
      "command": "npx",
      "args": ["-y", "telegram-mcp-bot@1.1.0"],
      "env": {
        "TELEGRAM_BOT_TOKEN": "${TELEGRAM_BOT_TOKEN}"
      }
    }
  }
}
```

> **"One `.mcp.json` works for every agent" is false.** Only Claude reads that file natively. OpenCode uses its own `mcp` block in `opencode.json`; Codex uses `mcp_servers` in `~/.codex/config.toml`; Pi reaches MCP servers through an adapter package (this project uses `pi-mcp-adapter`, pinned in `.pi/settings.json`). Each agent you want to send from is its own wiring job, in its own file, with its own syntax.

This is the strongest practical argument for the orchestrator-only design above. With one notifier, you wire **one** agent's MCP configuration and the other roles' divergent config formats simply stop being your problem.

One more naming trap: **the tool name depends on the client**. Reached through `pi-mcp-adapter` the tool is exposed server-prefixed as `telegram_send_message`; a client that discovers the server natively lists it unprefixed. A live send in this project failed on exactly that mismatch. So write your prompt to describe the tool by role — "the Telegram MCP's send-message tool" — and tell the agent to use whatever name its own client reports, rather than hard-coding `send_message` and hoping.

### Security requirements

These are requirements, not suggestions. Each one is a property of this class of setup rather than a hypothetical.

**Always pass an explicit `chat_id`.** The reviewed `telegram-mcp-bot@1.1.0` has **no allowed-user and no allowed-chat check**. Every inbound message updates that chat's last-active timestamp, and when `chat_id` is omitted the send tools fall back to the **most recently active chat**. So anyone who learns your bot's username (`@your_bot` is public the moment you use it) can message the bot, become the most-recent chat, and receive your next notification — which may be `needs input: <the thing you did not want to say out loud>`. Pass the id every time, from configuration; if it is unset, **skip the send** rather than falling back. Do not discover it at runtime from the server's chat-listing tool.

**Never let the agent read the inbound side.** An updates or inbox tool (`get_updates` on this server) is an **unauthenticated inbound channel**: anyone can put text in it. Feeding that to an agent with tool access is a prompt-injection path, and nothing you need to tell the orchestrator ever needs to arrive that way. Instruct the agent explicitly not to call it — a tool that exists will otherwise be tried.

**Every stdio MCP server sees your whole environment.** MCP hosts spawn stdio servers with the **full parent environment**, so each server you declare can read every secret in that shell — not just the one variable you wired into its `env` block. This is standard MCP-host behaviour rather than anything specific to this setup, but it is the thing to weigh before adding a third-party server to a shell that also holds your cloud credentials.

**Pin versions; do not track `@latest`.** `telegram-mcp-bot@1.1.0` above is pinned deliberately: this package receives your bot token and inherits your environment, so a silent upgrade is a silent change to code with that access. Be aware of what pinning does *not* buy you — an exact top-level pin does not pin the transitive dependency graph, and a server that ships no lockfile still resolves mutable, unreviewed dependencies that run in the same process with the same environment. Closing that properly means a locally installed server with a committed lockfile, installed with something like `npm ci --omit=dev --ignore-scripts`, rather than `npx`. Pinning the top level is a real improvement; it is not the whole fix.

**Expect the server to outlive a parent that dies uncleanly.** `telegram-mcp-bot@1.1.0` does not exit when the agent that started it goes away without shutting it down: its `uncaughtException` handler logs instead of exiting (deliberately, so a polling error does not disconnect the host), nothing in it exits when its stdin closes, and its Telegram polling loop keeps the process alive. So when that agent crashes or is killed, the server is left running on its own, still polling with your bot token. With both of its output pipes broken it can also spin: in this project one orphaned instance was measured at a full CPU core for 36 hours, and it did not stop on `SIGTERM` — `SIGKILL` ended it. One instance is enough; this does not depend on running several agents. After an agent crashes, look for a leftover: `ps -eo pid,ppid,etime,pcpu,args | grep '[t]elegram-mcp-bot'` shows each process's parent, age and CPU. The same listing also shows the servers of agents that are still running, and the `npx` process each one was started through, so check the parent before killing anything — `ps -o pid,args -p <ppid>` names it. A server whose parent is a live agent, or an `npx` process whose own parent is one, belongs to a running session; leave it alone. Anything else is a leftover: its parent — or, if its `npx` process survived too, that process's parent — is no longer an agent, typically PID `1` or a subreaper such as `systemd --user`. Kill the leftover server by its pid, and its `npx` process if there is one, with `kill -9` if a plain `kill` does not stop it.

### Where the chat id lives

The identifier has to reach the agent's environment somehow — an environment variable read from your shell is the obvious route:

```bash
export TELEGRAM_CHAT_ID=<your-chat-id>
```

A chat id is an **identifier, not a credential**: knowing it does not let anyone send as your bot, and it does not authorise anything on its own. It does name a private destination, though, so treat it the way you treat an internal email address — not committed to a public repo out of habit, not guarded like a token either. Where exactly it lives (shell profile, secret manager, `direnv` file, your agent's own config) is your call, and the deck neither reads nor stores it.

### What survives a compaction, and what does not

There is an asymmetry here worth understanding before you rely on any of this.

The escalation, merge-gate, and done notifications live in the **orchestrator's prompt**. On a long run that prompt can be compacted away — the instructions were context, and context is what compaction reclaims. The symptom is silence: the run reaches a gate and stops, correctly, but no message is sent, and you find out by wandering back to the terminal. Nothing errors, so nothing tells you it happened. See [issue #82](https://github.com/vfarcic/dot-agent-deck/issues/82) for how that mechanism is being addressed more generally.

The daemon's **idle-worker prompt does not have that failure mode**. It is injected fresh at the moment it fires and it is self-describing: it explains what it is and what the orchestrator might do about it, in its own text. An orchestrator that has forgotten every notification instruction still receives a coherent report and can still act sensibly on it.

So the part of this page that is a real feature degrades gracefully, and the part that is a recipe degrades silently. If that matters to you, keep a small local log — one appended line per notify moment, whether or not the send succeeded — which is enough to tell "the channel was down" from "the agent never tried". The second is the symptom of a compacted-away instruction, and it is otherwise invisible.
