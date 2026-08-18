# Experimental Flag

> **Developer / maintainer reference.** This page documents an internal development mechanism and is intentionally excluded from the published documentation site.

`dot-agent-deck` can hide in-flight, work-in-progress surfaces behind a single boolean feature flag named `experimental`. It is **off by default**, so a normal install never shows half-finished features. Enable it only when you want to test a surface that a PRD has explicitly marked as experimental.

## What the flag does

The flag is a **presentation switch**, not a behaviour switch. It controls only whether certain *new, user-visible surfaces* (a pane, field, command, tab, footer, or keybinding) are shown. The underlying code paths run regardless — the flag just decides whether you can see and reach the new surface.

A feature is gated by the flag only when its PRD says so. Surfaces that are not marked experimental are always visible and are unaffected by this flag.

## How to enable it

There are two ways to turn it on. **The environment variable wins over the file** — if it is set, the file value for this field is ignored.

**1. Config file (`.dot-agent-deck.toml`)**

Add a `[features]` table to your project's `.dot-agent-deck.toml` — the deck finds it from anywhere inside the project, not only at the top (see [Which file is read](#which-file-is-read)):

```toml
[features]
experimental = true
```

Editing this file while the deck is running takes effect live — within a couple of seconds, no restart needed. Set it back to `false` (or remove the table) to hide the experimental surfaces again.

**2. Environment variable (`DOT_AGENT_DECK_EXPERIMENTAL`)**

```bash
DOT_AGENT_DECK_EXPERIMENTAL=1 dot-agent-deck
```

The value is case-insensitive: `1` or `true` enables the flag; any other value (or leaving it unset) disables it.

> **Environment overrides the file.** When `DOT_AGENT_DECK_EXPERIMENTAL` is set, it decides the flag's state and edits to `[features] experimental` in `.dot-agent-deck.toml` are ignored until you unset the variable. Set the variable to `1`/`true` to force the experimental surfaces on regardless of the file, or to `0`/`false` to force them off.

## Which file is read

The deck starts at its launch directory and **walks up to the nearest ancestor holding a `.dot-agent-deck.toml`**, and that directory is the project. So launching from `repo/src`, `repo/crates/app`, or any other directory inside the project finds `repo`'s flags — the flag depends on which *project* you are in, not on which of its directories you happened to be standing in. The nearest config wins, so a nested project (a crate with its own config, a worktree checked out inside a repo) overrides the one above it.

Before issue #577 there was no walk and no explicit directory at all: the loader reached for `std::env::current_dir()` itself, which made it the only config read in the deck keyed to the process's own working directory rather than to one it was handed. Launching one level down from the project root was enough to read a `.dot-agent-deck.toml` that is not there, and every experimental surface silently resolved off with no way to tell that apart from the feature having been removed.

Two limits are deliberate:

- **A candidate must be a regular file you own.** Walking upward means considering directories you did not name and may not own — `/tmp/project` sits under a world-writable `/tmp`, where any local user can create `/tmp/.dot-agent-deck.toml`. An ancestor config owned by anyone else is skipped and the walk continues past it. (On Windows there is no uid, and the walk climbs through your ACL-protected user profile before reaching an admin-writable `C:\`, so any regular file is accepted there; the divergence is recorded at the check itself.)
- **If nothing is found at or above the launch directory, that directory is the answer** — byte-identical to the pre-#577 path. A deck launched entirely outside any project reads exactly what it read before.

That last point is the **residual**: a deck launched somewhere with no project above it — `$HOME`, say — with its panes pointed into a project elsewhere still does not pick up that project's flags. The `[features]` table is one process-global toggle (see "One flag for everything" below), and some gated surfaces — the dashboard footer, for one — belong to no project at all, so there is no per-pane answer to give. For that case set **`DOT_AGENT_DECK_FEATURES_CONFIG`** to the full path of the `.dot-agent-deck.toml` to read: it names the file outright and wins over the walk (the `DOT_AGENT_DECK_EXPERIMENTAL` env var still wins over both, since it decides the value rather than the file). This is also how the test suite keeps the flag off the real working directory.

The startup log line names the file it read (below), so "which `.dot-agent-deck.toml` did this deck actually load?" is answerable from the log rather than by inference.

## Default and precedence

| Source | Value | Result |
|---|---|---|
| Nothing set | — | **Off** (default) |
| `[features] experimental = true` in `.dot-agent-deck.toml` | file | On |
| `DOT_AGENT_DECK_EXPERIMENTAL=1` (or `true`) | env | On — wins over the file |
| `DOT_AGENT_DECK_EXPERIMENTAL=0` (or `false`/other) | env | Off — wins over the file |

Both the TUI and the background daemon read the flag independently from the same `.dot-agent-deck.toml`, so the two stay consistent — the file is the contract. On startup each process logs a single line — `experimental flag: ON (from /path/to/.dot-agent-deck.toml)` or the same with `OFF` — when file logging is enabled (`DOT_AGENT_DECK_LOG`). The path is the file the value came from, so a flag that resolves off because the wrong file was read is distinguishable from one that is simply set to false.

> **One flag for everything.** There is exactly one experimental toggle. If two unrelated experimental surfaces are in flight at once, they are shown or hidden together — there are no per-feature toggles.

## Why surfaces are gated

This lets work-in-progress code merge to `main` without exposing unfinished UI during normal use. Each gated surface is wired behind a small wrapper function so that, once the feature is finished ("graduates"), the gating is removed mechanically and the surface becomes visible to everyone. Until then, it stays behind `experimental`.

## Currently gated

| Wrapper (in `src/features.rs`) | Surface | PRD | Graduation |
|---|---|---|---|
| `show_experimental_footer()` | The experimental dashboard footer | #139 | — |
| `show_issue_dispatch_authoring()` | The new-pane `schedule: issues` modal authoring option (PRD #120 creation UX) | #120 | `graduate-issue-dispatch` |
| `show_command_entry_lock()` | The orchestration command-entry lock: the `Ctrl+E` binding, the keystroke gate on a focused worker pane, and the waiting-pane focus steering | #393 | `graduate-command-entry-lock` |

> **`show_command_entry_lock()` was added AFTER the feature merged (#404), not before it.** PRD #393 shipped un-gated and locked-by-default; the flag was added while it was still unreleased, so no version ever exposed it on. Three seams in `src/ui.rs` read the wrapper — the `Ctrl+E` binding resolution, the `PaneInput` keystroke gate, and the auto-focus chain. Note the third: the focus steering is part of the same surface rather than a separate feature, because it only ever ran while the lock was engaged, so gating it off is what makes flag-off behaviour identical to v0.35.8. The helpers themselves (`scope_command_entry_lock`, `gate_pane_input_key`) stay flag-free so their unit tests exercise the real logic rather than the gate; `UiState::command_entry_locked` also still starts `true`, since the flag decides whether that value is *consulted*, not what it is.

## Graduated

| Surface | PRD | Graduated |
|---|---|---|
| The new-pane `dispatcher` Mode-cycler option (PRD #220) — its `show_dispatcher()` wrapper is deleted and the branch inlined to `true`. The `dispatch` CLI verb and its daemon handler were never gated. Documented for users at [`docs/dispatcher-mode.md`](../dispatcher-mode.md). | #220 | in #220's own PR, before shipping |

> **`show_issue_dispatch_authoring()` is a render seam, like the others (redesigned 2026-06-24).** An earlier iteration gated `issue_dispatch` *behaviour* (the daemon's schedule-fire activation seam) — that is **gone**. A configured `issue_dispatch` task now runs **unconditionally**; the flag, config parsing, and the `schedule add --repo …` CLI are all flag-free. The wrapper now gates ONLY the new-pane Mode-cycler `schedule: issues` authoring option (a render/input seam in `src/ui.rs`) — i.e. the experimental *creation UX* for the task type, not the task type itself. This keeps the flag presentation-only, consistent with the default model.
