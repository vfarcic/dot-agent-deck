# Experimental Flag

> **Developer / maintainer reference.** This page documents an internal development mechanism and is intentionally excluded from the published documentation site.

`dot-agent-deck` can hide in-flight, work-in-progress surfaces behind a single boolean feature flag named `experimental`. It is **off by default**, so a normal install never shows half-finished features. Enable it only when you want to test a surface that a PRD has explicitly marked as experimental.

## What the flag does

The flag is a **presentation switch**, not a behaviour switch. It controls only whether certain *new, user-visible surfaces* (a pane, field, command, tab, footer, or keybinding) are shown. The underlying code paths run regardless — the flag just decides whether you can see and reach the new surface.

A feature is gated by the flag only when its PRD says so. Surfaces that are not marked experimental are always visible and are unaffected by this flag.

## How to enable it

There are two ways to turn it on. **The environment variable wins over the file** — if it is set, the file value for this field is ignored.

**1. Config file (`.dot-agent-deck.toml`)**

Add a `[features]` table to the `.dot-agent-deck.toml` in the directory where you launch the deck:

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

## Default and precedence

| Source | Value | Result |
|---|---|---|
| Nothing set | — | **Off** (default) |
| `[features] experimental = true` in `.dot-agent-deck.toml` | file | On |
| `DOT_AGENT_DECK_EXPERIMENTAL=1` (or `true`) | env | On — wins over the file |
| `DOT_AGENT_DECK_EXPERIMENTAL=0` (or `false`/other) | env | Off — wins over the file |

Both the TUI and the background daemon read the flag independently from the same `.dot-agent-deck.toml`, so the two stay consistent — the file is the contract. On startup each process logs a single line — `experimental flag: ON` or `experimental flag: OFF` — when file logging is enabled (`DOT_AGENT_DECK_LOG`).

> **One flag for everything.** There is exactly one experimental toggle. If two unrelated experimental surfaces are in flight at once, they are shown or hidden together — there are no per-feature toggles.

## Why surfaces are gated

This lets work-in-progress code merge to `main` without exposing unfinished UI during normal use. Each gated surface is wired behind a small wrapper function so that, once the feature is finished ("graduates"), the gating is removed mechanically and the surface becomes visible to everyone. Until then, it stays behind `experimental`.

## Currently gated

| Wrapper (in `src/features.rs`) | Surface | PRD | Graduation |
|---|---|---|---|
| `show_experimental_footer()` | The experimental dashboard footer | #139 | — |
| `show_issue_dispatch_authoring()` | The new-pane `schedule: issues` modal authoring option (PRD #120 creation UX) | #120 | `graduate-issue-dispatch` |

> **`show_issue_dispatch_authoring()` is a render seam, like the others (redesigned 2026-06-24).** An earlier iteration gated `issue_dispatch` *behaviour* (the daemon's schedule-fire activation seam) — that is **gone**. A configured `issue_dispatch` task now runs **unconditionally**; the flag, config parsing, and the `schedule add --repo …` CLI are all flag-free. The wrapper now gates ONLY the new-pane Mode-cycler `schedule: issues` authoring option (a render/input seam in `src/ui.rs`) — i.e. the experimental *creation UX* for the task type, not the task type itself. This keeps the flag presentation-only, consistent with the default model.
