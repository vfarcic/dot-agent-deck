---
name: docs-screenshots-review
description: 'Check that a change to the user-facing docs covers both clients (the TUI and the desktop app) unless the feature exists in only one, and decide whether it needs a new or updated screenshot, then produce it or say exactly which one to capture. Use whenever you create, edit, move or delete any file under docs/ that is NOT under docs/develop/ (the pages published to agent-deck.devopstoolkit.ai), or the home page (site/landing/), in any task, including when the docs edit is a side effect of a code change, a changelog-driven update or a one-line fix. Also use when a user-visible UI change (TUI or desktop app) makes an existing docs screenshot stale. Invoke it before you report the docs change as done.'
user-invocable: true
---

# Both clients, and screenshots, for docs changes

The user docs under `docs/` (not `docs/develop/`, which is never published) and the site's home page describe two clients of one daemon, the TUI and the desktop app. Screenshots are part of those docs, they go stale, and they are generated from code by `cargo docs-screenshots` so that fixing a stale one is a command, not a manual session. This skill makes sure every docs change covers both clients and gets a deliberate screenshot decision.

## 0. Show and explain both clients, unless only one has the feature

**The rule: every page shows and explains both the TUI and the desktop app, unless the feature works in only one of them.** Most features live in the daemon, so both clients have them; they just present them differently (a card or a row, a tab or a group, `Ctrl+n` or **New agent**). Name both where they differ, in the prose and in the screenshots. Do not write a page, a paragraph or a caption that describes only the TUI and leaves the desktop app implied, or the other way round.

A feature one client lacks is the exception, and it is stated, not left implicit: say which client has it (the Schedules manager and workspace modes are TUI-only; voice control and several daemons on one dashboard are desktop-only), so a reader of the other client knows why they do not see it. Check "both have it" against the code before writing it (CLAUDE.md rule 17) — the desktop feature inventory in `prds/done/1321-user-docs-both-clients.md` is a starting point, not the source of truth.

How to show both depends on the page. In the docs, a screenshot of a feature both clients have is one image per client, under a **`**TUI:**` and a `**Desktop:**` label** (section 2). On the home page, the reader should not have to choose a client, so both clients' images sit **side by side**, each labelled (the story rows in `site/landing/index.html` are the pattern).

## 1. Decide whether the change warrants a screenshot

Read the diff of the docs pages you touched and answer, for each changed section:

- **Does it describe a screen?** A new page, a new section about a surface (a pane, a dialog, a form, a settings panel, a status, a layout), or a changed flow the reader follows with their eyes. Prose about configuration files, CLI output, protocols or concepts usually needs no image.
- **Does an image already cover it, and is it still accurate?** Look at every image the section and its page embed (`![…](/img/…)` or `./img/…`) and open the image file. If the change you made — or the code change behind it — alters what that screen shows (a label, a column, a button, a status word), the image is stale.
- **Is it a feature both clients have?** Then a screenshot needs one image **per client**, depicting the same state: one labelled block per client in the docs, side by side on the home page (section 0).

Write the decision down in your report either way — "no screenshot needed: this section is about the TOML schema" is a valid outcome. Do not add images for their own sake.

## 2. If it does, produce it — preferably from a scenario

**`cargo docs-screenshots` first.** [`docs/develop/docs-screenshots.md`](../../../docs/develop/docs-screenshots.md) is the reference: prerequisites, the command, the registered scenarios, and how to add one. In short:

```bash
cargo docs-screenshots --list                    # what exists
cargo docs-screenshots --scenario <name>         # regenerate one into docs/img/
```

- **An existing scenario covers the screen:** regenerate it and embed `/img/<scenario>-<client>.png`.
- **None covers it:** add or extend a scenario, as that page's "Adding a scenario" section describes — an entry in `xtask/screenshots/src/scenarios.rs`, its TUI capture in `tests/e2e_docs_screenshots.rs`, its desktop capture in `desktop/screenshots/desktop.shot.ts` (with fixture state in `desktop/src/data/fixture.ts` when it must mirror a TUI scene). A feature both clients have uses **one scenario name on both**, depicting the same state, so the two images pair up.
- `docs/img` is a symlink to `site/static/img`; the PNGs are committed there. Never commit a screenshot that shows a real home path, host name, token or someone else's project — scenarios run in a sandbox for that reason.

**Embed a two-client screenshot as two labelled blocks**, TUI first. The published docs are plain Markdown, served as is and printed by `dot-agent-deck docs`, so there are no tabs or other MDX components:

```md
**TUI:**

![What the TUI shows, described](/img/<scenario>-tui.png)

**Desktop:**

![What the desktop app shows, described](/img/<scenario>-desktop.png)
```

The blank lines around each image are required. Write alt text that says what is in the frame.

**Where no scenario can reach the screen**, use the [`run-dot-agent-deck`](../run-dot-agent-deck/SKILL.md) skill to drive the TUI in an isolated sandbox and capture it, and say on the page, in the PRD or in your report that this image is not reproducible from a scenario and why.

That sandbox isolates the capture from your own deck; it redacts nothing. A frame with a real agent in it can show credentials, prompts or someone else's content, so apply the rule for agent-backed scenarios in [`docs/develop/docs-screenshots.md`](../../../docs/develop/docs-screenshots.md): redact before the image is written, then inspect the final image yourself for tokens, real home paths, host names, user names, prompts and anyone else's content before committing or publishing it. If it cannot be made clean, do not commit it; tell the user what to capture instead, as section 3 describes.

Then build the site — `cargo xtask site <a-new-directory>` — which fails on a broken image or link.

## 3. If you cannot produce it, say exactly what to capture

When producing it is out of reach (no display, the desktop app's Playwright dependencies missing, a screen that needs a real agent or a remote host), do not skip silently. Tell the user, in your final report: the page and section, the client(s), the exact state to show (which screen, which dialog open, what data on screen), the file name it should have (`docs/img/<scenario>-<client>.png`), and whether a scenario should be added for it.

## Why a skill and not a hook

Most tasks never touch `docs/` or the home page, so this is a skill rather than a `CLAUDE.md` rule that every session would load. If agents turn out not to invoke it reliably, the fallback is a hook scoped to edits under `docs/**` that prompts for this review. That hook is **not built**; it is the next step only if this skill is observed being skipped (PRD #1321).
