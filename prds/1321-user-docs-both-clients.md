# PRD #1321: User docs for both clients — common, TUI and desktop sections, with screenshots

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-27
**Issue**: [#1321](https://github.com/vfarcic/dot-agent-deck/issues/1321)
**Absorbs**: [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) (document installing the desktop GUI) — the PR that completes this PRD closes #765
**Depends on**: [#1045](https://github.com/vfarcic/dot-agent-deck/issues/1045) (one glossary), [#746](https://github.com/vfarcic/dot-agent-deck/issues/746) (logo), [#757](https://github.com/vfarcic/dot-agent-deck/issues/757) (signed desktop artifacts) — all three closed as of 2026-09-27
**Uses**: [#1322](https://github.com/vfarcic/dot-agent-deck/issues/1322) (`cargo docs-screenshots`, closed as of 2026-09-27) — not a blocker
**Interacts with**: [#1308](https://github.com/vfarcic/dot-agent-deck/issues/1308), [#1309](https://github.com/vfarcic/dot-agent-deck/issues/1309), [#1310](https://github.com/vfarcic/dot-agent-deck/issues/1310), [#1311](https://github.com/vfarcic/dot-agent-deck/issues/1311), [#1312](https://github.com/vfarcic/dot-agent-deck/issues/1312) (the desktop graduations, open as of 2026-09-27), [#176](https://github.com/vfarcic/dot-agent-deck/issues/176) (the desktop GUI PRD whose user-doc item was left unchecked)

## Problem Statement

The published docs site (`docs/`, served at agent-deck.devopstoolkit.ai) describes the TUI only. The desktop app has no user-facing documentation: PRD #176 closed with M5.3's "user doc once past spike quality" unchecked, and #765 deferred the install page until the app left unsigned alpha. The desktop app appears only in passing in five pages (configuration, troubleshooting, remote environments, remote requirements, idle workers). Separately, the docs as a whole are mostly text: 5 of 15 pages carry any image.

With the first signed desktop release shipped by #757 (v0.42.0, a Developer ID signed and notarized macOS `.dmg`), the desktop app becomes something we want people to install on purpose, and the docs need to present both clients.

## Solution Overview

Restructure the user docs around the fact that the TUI and the desktop app are two clients of the same daemon, document the desktop app, and add screenshots throughout. Six parts:

### 1. Analyse the desktop features from the code first

Before writing any page, build an inventory of the desktop app's user-visible features **from the code** (`desktop/src/`, `desktop/src-tauri/`), not from the developer docs. Reading `docs/develop/desktop-gui.md` and the desktop PRDs is fine as context, but they are not the source of truth. For each feature, record where it lives in the code, whether the TUI has the same feature, and whether it is behind the `experimental` flag. The inventory is checked into this PRD (see [Desktop feature inventory](#desktop-feature-inventory)); it drives the page structure and the screenshot list. It is taken when the work starts (milestone 1), not when this document is written, because the feature set is still moving.

### 2. Split the docs into three groups

- **Common**: features both clients have. They are both daemon clients, so this should be most of it.
- **TUI-specific**.
- **Desktop-specific**.

`site/sidebars.js` is organised to match.

### 3. Home page features both clients

The docs site home page (`site/src/pages/index.js`) presents both the TUI and the desktop app.

### 4. Install page

The install page **absorbs #765**: which desktop artifact to download for which platform, per-platform install and first launch, how the app finds or starts a daemon, and how the GUI relates to the TUI (same daemon, second client). It describes the signed install that #757 delivered. The PR that completes this PRD closes #765.

### 5. Screenshots

- Screenshots are added across the docs in general, not only to the desktop part.
- A feature common to both clients that is illustrated with a screenshot gets one from **each** client, shown as tabs (TUI | Desktop) so the page does not double in length.
- Screenshots go stale, and that is accepted; the work does not wait for a final version.
- **Screenshots come from `cargo docs-screenshots`** (issue #1322, [`docs/develop/docs-screenshots.md`](../docs/develop/docs-screenshots.md)), not from hand captures. Each new screenshot is a new or extended scenario in that tool's registry (`xtask/screenshots/src/scenarios.rs`, with its TUI capture in `tests/e2e_docs_screenshots.rs` and its desktop capture in `desktop/screenshots/desktop.shot.ts`), so it can be regenerated when the UI changes. A feature both clients have uses the **same** scenario name on both, depicting the same state, which is what makes the TUI | Desktop tabs possible (`dashboard` is the worked example). A screen the tool genuinely cannot reach is the exception, and is recorded as such where it is used.

### 6. A skill that makes agents consider screenshots whenever they change docs

Add a **project-local skill** under `.claude/skills/`, **without the `dot-ai-` prefix** (CLAUDE.md rule 13: `dot-ai-*` skills are a synced mirror and a project edit there is overwritten). It is a skill rather than a `CLAUDE.md` rule, since most work does not touch docs and `CLAUDE.md` is loaded in full every session. Its description should trigger on any edit to user-facing docs under `docs/`, in any task, not only this PRD. The instruction: evaluate whether the change warrants a new or updated screenshot, and if it does, either produce it (through `cargo docs-screenshots`, or the TUI via the `run-dot-agent-deck` skill where no scenario fits) or tell the user exactly which screenshot to capture. If agents turn out not to invoke it reliably, a hook scoped to edits under `docs/**` is the fallback; that hook is not built up front.

## Desktop feature inventory

*Filled in by milestone 1, from the code, when the work starts.* Deliberately empty in this draft: the feature set is still moving (#1308–#1312 are open), and an inventory taken now would be stale before it drove anything.

For each feature the inventory records:

| Feature | Where it lives in the code | TUI has it? | Behind `experimental`? |
| --- | --- | --- | --- |
| *(milestone 1)* | | | |

The last column decides what gets documented (see Constraints), and the docs say which desktop features are behind `experimental` according to it.

## Scope

### In scope

- The desktop feature inventory, from the code, checked into this PRD.
- Restructuring `docs/` into common / TUI / desktop groups and reorganising `site/sidebars.js` to match.
- Desktop pages for every graduated desktop feature.
- The install page, absorbing #765.
- The home page presenting both clients.
- Screenshots across the docs, generated through `cargo docs-screenshots`, with TUI | Desktop tabs for common features.
- The project-local screenshot skill.
- Publishing the docs (`publish-docs`).

### Out of scope

- **User docs for experimental features.** They are documented when they graduate, not before.
- **Any code surface in either client, and any daemon or protocol change.** This is a docs PRD.
- **A hook enforcing the screenshot skill.** It is the fallback if the skill proves unreliable, not part of this work.
- **Developer docs.** They stay under `docs/develop/` (CLAUDE.md rule 11), except where the screenshot tool's own page needs a scenario documented.

## Constraints

- **Do not document experimental features.** Only surfaces that have graduated from the `experimental` flag by the time this work runs get user docs (see #1308–#1312 for the current desktop graduations). The docs must say which desktop features are behind `experimental`, per the inventory, so a user who does not see one knows why.
- **Terminology follows the glossary from #1045**: the TUI's words, except where the TUI itself is inconsistent, in which case the user decides.
- User docs go under `docs/`; developer docs stay under `docs/develop/` and are never added to `site/sidebars.js` (CLAUDE.md rule 11).
- No hard-wrapped Markdown prose: one line per paragraph (CLAUDE.md rule 10).
- **CLAUDE.md rule 9 (experimental flag) does not apply.** This PRD adds no user-visible code surface — no pane, field, command, tab, footer or keybinding — so there is nothing to gate and no `show_<feature>()` wrapper to add.
- **CLAUDE.md rule 12 (daemon contract) does not apply.** This PRD changes no daemon, TUI↔daemon protocol, orchestration or hook code, so there is no `PROTOCOL_VERSION` question, no `.breaking.md` fragment and no cross-version manual test.
- Absolutes in the new pages are checked against the code before they are written (CLAUDE.md rule 17), which matters most for the "both clients have it" claims the common group rests on.

## Dependencies and sequencing

- **#1045** (one glossary): must land first, so the docs use settled terms. Closed as of 2026-09-27.
- **#746** (logo): should land first, so screenshots and the app icon do not immediately go stale. Closed as of 2026-09-27.
- **#757** (signed desktop artifacts): the install page describes the signed install. Closed as of 2026-09-27; v0.42.0 shipped the first signed `.dmg`.
- **#1322** (generating screenshots of both clients from code): not a blocker. Closed as of 2026-09-27; `cargo docs-screenshots` is the tool this PRD uses.
- **#1308–#1312** (desktop graduations): not blockers, but they decide which desktop surfaces the docs may cover when the inventory is taken.

Target: the release after the one carrying #1045 and #746.

## Milestones

- [ ] **M1** — Desktop feature inventory from the code, checked into this PRD
- [ ] **M2** — Docs restructured into common / TUI / desktop, sidebar updated
- [ ] **M3** — Desktop pages written for every graduated desktop feature
- [ ] **M4** — Install page (absorbs #765)
- [ ] **M5** — Home page presents both clients
- [ ] **M6** — Screenshots added across the docs via `cargo docs-screenshots`, with TUI | Desktop tabs for common features
- [ ] **M7** — Project-local skill for evaluating screenshots on docs changes
- [ ] **M8** — Docs published (`publish-docs`)

## Risks

- **The inventory goes stale while the pages are written.** Graduations (#1308–#1312) may land mid-work. Mitigation: take the inventory at the start, as the issue requires, and re-check the `experimental` column before publishing.
- **Screenshots go stale.** Accepted by design; generating them from scenarios keeps regeneration a command away.
- **A "common" claim is wrong.** A feature documented as shared that one client lacks misleads users of that client. Mitigation: the inventory's "TUI has it?" column is the source of the common group, and it comes from the code.
- **Agents do not invoke the screenshot skill.** Mitigation: the `docs/**` hook named in section 6, built only if this is observed.

## Success criteria

- Every graduated desktop feature has user docs, and no experimental one does.
- A user can install and first-launch the desktop app on each published platform from the install page alone, and #765 is closed.
- The home page and sidebar present the TUI and the desktop app as two clients of one daemon.
- Common features illustrated with a screenshot show both clients as tabs, and every screenshot is reproducible with `cargo docs-screenshots`.
- An agent editing `docs/` in an unrelated task is prompted by the skill to consider a screenshot.

## Refs

#1322, #176, #765, #757, #746, #1045, #1308, #1309, #1310, #1311, #1312

## Work Log

### 2026-09-27 — Created

Document written from issue #1321's body. The desktop feature inventory is left as a placeholder for M1, as the issue requires.
