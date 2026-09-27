---
name: docs-screenshots-review
description: 'Decide whether a change to the user-facing docs needs a new or updated screenshot, and produce it or say exactly which one to capture. Use whenever you create, edit, move or delete any file under docs/ that is NOT under docs/develop/ (the pages published to agent-deck.devopstoolkit.ai), in any task, including when the docs edit is a side effect of a code change, a changelog-driven update or a one-line fix. Also use when a user-visible UI change (TUI or desktop app) makes an existing docs screenshot stale. Invoke it before you report the docs change as done.'
user-invocable: true
---

# Screenshots for docs changes

The user docs under `docs/` (not `docs/develop/`, which is never published) describe two clients of one daemon, the TUI and the desktop app. Screenshots are part of those docs, they go stale, and they are generated from code by `cargo docs-screenshots` so that fixing a stale one is a command, not a manual session. This skill makes sure every docs change gets a deliberate screenshot decision.

## 1. Decide whether the change warrants a screenshot

Read the diff of the docs pages you touched and answer, for each changed section:

- **Does it describe a screen?** A new page, a new section about a surface (a pane, a dialog, a form, a settings panel, a status, a layout), or a changed flow the reader follows with their eyes. Prose about configuration files, CLI output, protocols or concepts usually needs no image.
- **Does an image already cover it, and is it still accurate?** Look at every image the section and its page embed (`![…](/img/…)` or `./img/…`) and open the image file. If the change you made — or the code change behind it — alters what that screen shows (a label, a column, a button, a status word), the image is stale.
- **Is it a feature both clients have?** Then a screenshot needs one image **per client**, shown as tabs (below), depicting the same state.

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

**Embed a two-client screenshot as tabs**, so the page does not double in length:

```mdx
import Tabs from '@theme/Tabs';
import TabItem from '@theme/TabItem';

<Tabs groupId="client">
<TabItem value="tui" label="TUI">

![What the TUI shows, described](/img/<scenario>-tui.png)

</TabItem>
<TabItem value="desktop" label="Desktop">

![What the desktop app shows, described](/img/<scenario>-desktop.png)

</TabItem>
</Tabs>
```

The imports go right below the frontmatter, the blank lines around each image are required, and `groupId="client"` keeps the reader's choice across the whole site. Write alt text that says what is in the frame. Pages are compiled as MDX, so `{` and `<` in prose must be inside code spans.

**Where no scenario can reach the screen**, use the [`run-dot-agent-deck`](../run-dot-agent-deck/SKILL.md) skill to drive the TUI in an isolated sandbox and capture it, and say on the page, in the PRD or in your report that this image is not reproducible from a scenario and why.

Then build the site — `cd site && npm ci && npm run build` — which fails on a broken image or link.

## 3. If you cannot produce it, say exactly what to capture

When producing it is out of reach (no display, the desktop app's Playwright dependencies missing, a screen that needs a real agent or a remote host), do not skip silently. Tell the user, in your final report: the page and section, the client(s), the exact state to show (which screen, which dialog open, what data on screen), the file name it should have (`docs/img/<scenario>-<client>.png`), and whether a scenario should be added for it.

## Why a skill and not a hook

Most tasks never touch `docs/`, so this is a skill rather than a `CLAUDE.md` rule that every session would load. If agents turn out not to invoke it reliably, the fallback is a hook scoped to edits under `docs/**` that prompts for this review. That hook is **not built**; it is the next step only if this skill is observed being skipped (PRD #1321).
