---
name: docs-screenshots-review
description: 'Check that a change to the user-facing docs covers both clients (the TUI and the desktop app) unless the feature exists in only one, keep screenshots out of the agent-facing docs pages, and keep the landing page''s and the installation page''s screenshots current. Use whenever you create, edit, move or delete any file under docs/ that is NOT under docs/develop/ (the pages published to agent-deck.devopstoolkit.ai), or the home page (site/landing/), in any task, including when the docs edit is a side effect of a code change, a changelog-driven update or a one-line fix. Also use when a user-visible UI change (TUI or desktop app) makes a screenshot on the landing page or in docs/installation.md stale. Invoke it before you report the docs change as done. Screenshots of a change for its pull request are pr-create''s, not this skill''s.'
user-invocable: true
---

# Both clients, and the few screenshots, for docs changes

The user docs under `docs/` (not `docs/develop/`, which is never published) and the site's home page describe two clients of one daemon, the TUI and the desktop app. This skill makes sure every docs change covers both clients, and that screenshots appear only where a person looks at them.

**Who reads what** (PRD #1419, issue #2438). The published docs are written for the user's agent: they are served as raw Markdown, through `llms.txt` and `llms-full.txt`, and by `dot-agent-deck docs`. An agent reads an image's alt text, not the picture, so **the docs pages carry no screenshots**: what a user sees is said in words. Two pages are read by people and keep theirs: the **landing page** (`site/landing/index.html`) and **`docs/installation.md`**, which the landing page links as GitHub renders it. The maintainer reviews both whenever they change. A person reviewing a UI change sees it in the **pull request**, whose screenshots the [`pr-create`](../pr-create/SKILL.md) skill asks for, **always in both the light and the dark theme**, side by side (PR #2491).

## 0. Show and explain both clients, unless only one has the feature

**The rule: every page explains both the TUI and the desktop app, unless the feature works in only one of them.** Most features live in the daemon, so both clients have them; they just present them differently (a card or a row, a tab or a group, `Ctrl+n` or **New agent**). Name both where they differ. Do not write a page or a paragraph that describes only the TUI and leaves the desktop app implied, or the other way round.

A feature one client lacks is the exception, and it is stated, not left implicit: say which client has it, so a reader of the other client knows why they do not see it. Known examples when this was last checked: the Schedules manager is TUI-only; voice control and several daemons on one dashboard are desktop-only. Treat that list, and the desktop feature inventory in `prds/done/1321-user-docs-both-clients.md`, as a starting point that goes stale as features move, not the source of truth: check "both have it" or "only one has it" against the code before writing it (CLAUDE.md rule 17).

On the landing page, the reader should not have to choose a client, so both clients' images sit **side by side**, each labelled (the story rows in `site/landing/index.html` are the pattern). In `docs/installation.md`, one image per client goes under a `**TUI:**` and a `**Desktop:**` label, TUI first, with a blank line around each image.

## 1. Describe the screen in words, not with an image

On every published page except `docs/installation.md`, do not add an image. Where a section is about a screen (a pane, a dialog, a form, a status, a layout), say what the user sees in it: the labels, buttons, states and where they are, for each client that has it. That sentence is what the reader acts on, and it does not go stale silently the way a picture does, because it is text a reviewer reads in the diff.

## 2. Keep the landing page's and the installation page's screenshots current

When the change, or the code change behind it, alters a screen one of those two pages shows (a label, a column, a button, a status word), regenerate that image. List what they use with:

```bash
grep -oE '/img/[A-Za-z0-9._-]+' site/landing/index.html docs/installation.md | sort -u
```

**Images named `<scenario>-<client>.png` come from `cargo docs-screenshots`**; [`docs/develop/docs-screenshots.md`](../../../docs/develop/docs-screenshots.md) is the reference.

**Light and dark, and what that means for these two pages.** Every screenshot of a user-visible change is captured in both themes, `<scene>-<client>-light.png` and `<scene>-<client>-dark.png` (PR #2491; [Light and dark](../../../docs/develop/docs-screenshots.md#light-and-dark)). That rule is for the screenshots that show a change, in its pull request. The landing page and `docs/installation.md` keep their existing images: one per client, in the dark theme, under the theme-less names they reference. So regenerate one of them with `--single-theme dark`, which writes that name in that theme; a plain run writes the light and dark pair under other names, which those pages do not use. Showing these pages' images in pairs, or following the landing page's own theme toggle, would be a change to the pages, for the maintainer to ask for, not something to do while refreshing an image. A PR that regenerates one still shows the changed screen in both themes in its body, as `pr-create` says.

```bash
cargo docs-screenshots --list
cargo docs-screenshots --scenario <name> --client <tui|desktop> --single-theme dark   # into docs/img, which is site/static/img
```

Regenerate only the images those pages use: `--scenario` and `--client` narrow the run, and an image nothing references is not committed (`git status` after the run shows any the command wrote besides). Whatever fonts the machine has are fine; the maintainer reviews these pages on every change. Look at each image before committing it, and at the light version of the screen too (a plain run into a scratch `--out`), since the PR shows both, and never commit one that shows a real home path, host name, token or someone else's project.

**The other images are hand-made** (`busy-deck-real.webp`, `dispatch.webp` and the like). When one goes stale, tell the user in your report: the page, the image, what it should show now.

Then build the site, `cargo xtask site <a-new-directory>`, which fails on a broken image or link.

## 3. Report

Say in your report which of the two pages' images you regenerated, or that the change alters none of them.

## Why a skill and not a hook

Most tasks never touch `docs/` or the home page, so this is a skill rather than a `CLAUDE.md` rule that every session would load. If agents turn out not to invoke it reliably, the fallback is a hook scoped to edits under `docs/**` that prompts for this review. That hook is **not built**; it is the next step only if this skill is observed being skipped (PRD #1321).
