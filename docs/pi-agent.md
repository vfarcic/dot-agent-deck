---
sidebar_position: 5.6
title: Pi Agent
---

# Pi Agent

[Pi](https://github.com/earendil-works/pi) is a third, first-class agent alongside `claude` and `opencode`. Unlike those two — which `dot-agent-deck` observes from the outside — Pi exposes a TypeScript extension API, so `dot-agent-deck` ships a small extension that gives the Pi pane **native `delegate`/`work-done` tools** and **event-driven status**. The result is a more deterministic orchestrator: instead of relying on an agent remembering to type a CLI command, the Pi orchestrator calls a validated tool whose body runs that command for it.

Pi is **detected on your PATH** like the other agents — `dot-agent-deck` does not bundle or vendor Pi (only the small extension is compiled into the binary). Using it is opt-in only in the sense that you must install `pi` and point a role at `command = "pi"`; there is no feature flag to enable.

> **Tested against Pi 0.80.6.** Pi is a young, fast-moving project. This integration is pinned to and tested against **Pi 0.80.6**; newer versions may change the extension API.

## What you get

- A Pi orchestrator that calls native `delegate(role, task)` and `work-done(summary)` tools, routed through the daemon to worker panes exactly like the existing CLI path.
- Live status (running / waiting-for-input / finished) for a Pi pane in the TUI and GUI — **with no Claude Code hook installed and no `~/.claude/settings.json` mutation**. The extension reports status directly from Pi's event bus.
- A plain `pi` pane opened from the dashboard, and a **scheduled** `pi` job, are status-tracked the same way — including unattended, with no client attached.

## Setup (one time)

**1. Install `dot-agent-deck`** as usual (see [Installation](installation.md)).

**2. Install Pi.** Pi needs a Node.js (or Bun) runtime. Install it once, like you would `claude` or `opencode`:

```bash
npm install -g @earendil-works/pi-coding-agent
```

This puts a `pi` binary on your PATH. Configure Pi's model provider and credentials per [Pi's own documentation](https://github.com/earendil-works/pi) — for example an OpenRouter or Anthropic API key in the environment. `dot-agent-deck` does not manage Pi's credentials.

**3. Run the setup command.** This detects `pi`, materializes the bundled orchestrator extension into Pi's extension directory (`~/.pi/agent/extensions/dot-agent-deck/`), and enables it:

```bash
dot-agent-deck orchestrator setup
```

If `pi` is not on your PATH, the command prints the one-line install hint above and exits without changing anything, so you can install Pi and re-run it.

**4. Point a role at Pi.** In your project's `.dot-agent-deck.toml`, set the orchestrator role's command to `pi` (add whatever provider/model flags Pi needs):

```toml
command = "pi --provider openrouter --model openai/gpt-5-nano"
```

See [Orchestration](orchestration.md) for the full role/config shape. Everything else about running an orchestration is unchanged — only the orchestrator pane is now Pi.

## Security and sandboxing

Pi runs with a **YOLO / no-permission model** — like Claude Code with full filesystem and shell access, it executes its tools without prompting. `orchestrator setup` and Pi's `--approve` flag trust the project so the `delegate`/`work-done` tools run without a permission dialog. This is the **same posture as the other agents** `dot-agent-deck` already spawns and does not change the deck's sandbox story: if you do not fully trust the workload, run `dot-agent-deck` (and therefore its agents) inside a container or other sandbox. See the security notes in [Getting Started](getting-started.md) — they apply to Pi exactly as they do to `claude` and `opencode`.

## What it deliberately does not do

- It does **not** bundle or vendor the Pi or Node/Bun runtime — Pi is detected on PATH; only the extension ships inside the binary.
- It does **not** replace `claude` or `opencode`, and does **not** remove hooks for those agents — the hook-free path is Pi-only.
- It does **not** adopt Pi's own multi-agent orchestration (TEAM/CHAIN/PIPELINE). `dot-agent-deck`'s daemon remains the orchestrator of record; Pi is a better-behaved node inside it.
