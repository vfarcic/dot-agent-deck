---
title: Workspace Modes (removed)
---

# Workspace Modes (removed)

Workspace modes — the `[[modes]]` blocks in `.dot-agent-deck.toml` that opened a tab with an agent pane beside persistent and reactive side panes — were removed in [#1199](https://github.com/vfarcic/dot-agent-deck/issues/1199).

**An existing `[[modes]]` block is ignored with a warning, not rejected.** The rest of the file still loads: the TUI shows a short status-line warning the first time the New Agent form reads that project's config in a session, and `dot-agent-deck validate` reports one warning scoped to `[[modes]]` while still exiting `0`. Delete the block to clear the warning. A saved session pane that belonged to a mode tab comes back as a plain pane on the dashboard, with a warning.

The **Mode** field in the New Agent form is still there. It offers `No mode`, your project's orchestrations, and the built-in `schedule` and `dispatcher` options — none of which were workspace modes, and none of which changed.

## What to use instead

- [Orchestration](orchestration.md) — several agents in one tab, each in its own pane, with an orchestrator delegating work between them.
- [Scheduled Tasks](scheduled-tasks.md) — run an agent on a cron schedule.
- [Dispatcher Mode](dispatcher-mode.md) — start work in an isolated copy of the repository without leaving what you are doing.

The pane interaction and scaffolding sections that used to live on this page moved: reading and typing into a pane is under [Keyboard Shortcuts](keyboard-shortcuts.md#reading-and-typing-into-a-pane), and `init`, config generation, `validate` and `watch` are under [Configuration](configuration.md#scaffolding).
