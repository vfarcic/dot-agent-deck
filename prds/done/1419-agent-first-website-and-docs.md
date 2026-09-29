# PRD #1419: Agent-first website and docs

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-29
**Issue**: [#1419](https://github.com/vfarcic/dot-agent-deck/issues/1419)

## Problem Statement

Everyone who uses dot-agent-deck is, by definition, already running an AI coding agent. The website at `agent-deck.devopstoolkit.ai` does not use that fact. It is a Docusaurus site of 20 user pages (about 4,000 lines, six of them under `docs/desktop/`) written for a person to read and navigate, and most of what those pages describe (writing `.dot-agent-deck.toml`, defining orchestration roles, setting up remote environments, scheduling tasks) is work a user would rather hand to the agent sitting next to the deck.

The agent cannot pick that work up reliably today, for two reasons:

- **It does not know the project exists.** dot-agent-deck is niche and newer than most models' training data. Asked "set up dot-agent-deck orchestration for me", an agent either guesses or searches, and the site it finds is laid out for human navigation (JavaScript-rendered pages, sidebars, theme chrome) rather than for reading.
- **The docs it could find may not match the installed version.** The site tracks the latest release, while a user's binary may be several releases older. A person reading a stale page notices; an agent executes it literally.

## Solution Overview

Make the website a landing page for people and make the docs a reference for the user's agent.

1. **A single landing page** for people, ported from the product page #1021 designed and PR #1155 shipped: what the tool is, why it is useful, a short demo, the one-line install, and a copy-paste prompt that hands the user's agent a concrete starting point ("Read https://agent-deck.devopstoolkit.ai/llms.txt, then help me install and set up dot-agent-deck").
2. **Docs published as raw Markdown**, linked from the landing page with a plain `<a href>` in the served HTML. A person can still read them; they are not designed for that.
3. **`llms.txt` and `llms-full.txt`**, generated from `docs/` at build time: an index an agent can follow, and the whole corpus in one fetch.
4. **`dot-agent-deck docs [topic]`**, a subcommand that prints docs embedded in the binary at build time, so an agent working with an installed deck reads the docs **for that version**, with no network access needed.
5. **The docs rewritten for their new reader.** They are still **user-facing docs**, but the user's agent is now the one reading them. See Decision 3.

## Audience

| Surface | Primary reader | Written for |
| --- | --- | --- |
| Landing page | A person evaluating or installing the tool | Deciding to try it, installing it, handing off to their agent |
| `docs/*.md` (published, embedded) | The **user's** agent, acting for the user | Installing, configuring, using and troubleshooting dot-agent-deck |
| `docs/develop/*.md` (unpublished) | An agent (or person) **working on this repository** | Changing the code — unchanged by this PRD |

## Scope

### In Scope

- Replace the Docusaurus site with a static landing page plus published Markdown, removing Node from the site build (Decision 1).
- A published-docs manifest (`docs/published.toml`) that is the single list of published pages, their titles and one-line descriptions (Decision 5).
- Update the `site_image_refs` linkage-check rule (#1200), which scans `site/src/` today, to follow the landing page to its new location.
- Landing-page imagery: the "busy deck" screenshot from `.landing-assets/` (retaken with `experimental` off, or cropped, and with personal paths removed) on the landing page; the "dispatch" screenshot, cleaned the same way and without its annotation arrow, as an illustration in the dispatcher-mode doc.
- Serve `.md` with a readable content type on **both** deploy targets `docs-publish.yml` feeds: the nginx image (`site/nginx-default.conf`, rolled out by Argo CD from `site/helm/`) and Netlify (`site/netlify.toml`).
- `llms.txt` / `llms-full.txt` generation, listing every published page and nothing from `docs/develop/`.
- `dot-agent-deck docs` (list topics) and `dot-agent-deck docs <topic>` (print one page), with the content embedded at build time.
- Rewrite the 20 user pages for the agent reader, per Decision 3.
- Redirects from every URL the Docusaurus build serves today to its Markdown successor, on **both** deploy targets, so existing inbound links (the README, past changelogs and release notes, search results) keep resolving. That means both the slashless and trailing-slash form of each page (`/docs/configuration` and `/docs/configuration/`), the nested desktop pages (`/docs/desktop/voice`), the category index (`/docs/desktop` → `desktop/index.md`), and `/docs` itself (→ a docs index, e.g. `llms.txt`). The redirect table is generated from the manifest rather than written by hand, and a test checks it against two lists: the URLs the last Docusaurus build emitted (captured before that build is deleted) and every `agent-deck.devopstoolkit.ai/docs/…` URL found in the repository. `/docs/workspace-modes` is already a 404 today (the page was removed but three `CHANGELOG.md` entries still link to it); redirect it to its closest successor or accept it as already dead, and say which.
- A link check that replaces Docusaurus's `onBrokenLinks: 'throw'`, which goes away with Docusaurus.
- Build-enforced guarantees that `docs/develop/` is neither published nor embedded (Decision 5).
- Repo-rule updates the change invalidates: CLAUDE.md rule 11 (describes the Docusaurus `exclude` glob and `site/sidebars.js`), the `publish-docs` skill, and `CONTRIBUTING.md`'s "excluded from the Docusaurus build" wording.
- A changelog fragment (the change is user-observable, per rule 19).

### Out of Scope

- **Developer docs (`docs/develop/`).** They are already written for agents and reached from the checkout via `CLAUDE.md`/`AGENTS.md`; publishing them would help no one who reads them. They are touched only in the one way Decision 3 requires: internals removed from user docs that are not already recorded there move there rather than being deleted. Condensing or restructuring the dev docs (or `CLAUDE.md`) is a separate issue if wanted.
- **A packaged Agent Skill / plugin** distributing the docs. A possible follow-up once the Markdown and `llms.txt` exist.
- **In-app TUI help** as a replacement for the keyboard-shortcuts page. The page stays in the published docs; whether the TUI should show it is a separate question.
- **Any daemon, protocol or desktop change.** `docs` is a local CLI subcommand that reads embedded content; it never contacts a daemon, so rule 12's cross-version check does not apply.

## Technical Approach

### Decision 1 — Drop Docusaurus; the landing page becomes static HTML and CSS (decided 2026-09-29)

Once the docs are served as Markdown, Docusaurus's only job would be rendering one page, and that page does not need it. `site/src/pages/index.js` (#1155) uses nothing from Docusaurus except `@theme/Layout` (navbar and footer) and `@docusaurus/Link`; it has no state, no event handlers and no tabs, so it is already a static page written in React. Its design lives in `index.module.css` (about 1,000 lines) and its text in `site/src/data/landing-content.js`, and both port to plain HTML and CSS mechanically. The design work from #1021 is kept, not redone.

What removing it buys:
- **No Node in the site build.** Seven npm dependencies go, with their Renovate churn, the `npm ci` stage in `site/Dockerfile`, and the `netlify-build` job in `docs-publish.yml`, which exists only to keep npm install scripts away from the Netlify token.
- **A build that is only generation and copying**: the published Markdown, `llms.txt`/`llms-full.txt`, images and the landing page. The generator is a `cargo xtask` that reads the same manifest as the `docs` subcommand (Decision 5), so one parser serves the site and the binary.

What it costs, all one-off:
- Porting the page and giving it its own header and footer. #1021's Task 1 already required one candidate to abandon the Docusaurus theme, so this is a direction that issue anticipated.
- Light and dark mode: #1021 required both, and the Docusaurus theme toggle goes. Use `prefers-color-scheme`, with a few lines of script if an explicit toggle is kept.
- The `site_image_refs` rule and `site/Dockerfile`, `site/netlify.toml`, `site/nginx-default.conf` and `docs-publish.yml` all follow the new layout. The delivery path itself (GHCR image → `site/helm` → Argo CD, plus Netlify, plus `/publish-docs`) stays.
- Docusaurus front matter (`sidebar_position`, `title`) is removed from all 20 pages; the manifest carries titles and descriptions instead, and each page's first heading is its title.
- `docs/img` is a symlink to `../site/static/img`. The published Markdown and the embedded copy must resolve images the same way, or the embedded copy must drop them.

### Decision 2 — Markdown is served readable, and the landing page links to it in plain HTML

- `.md` is served as `text/markdown; charset=utf-8`, so a browser shows it instead of downloading it and a fetching agent gets a type it recognises. This is configured on both deploy targets.
- The docs link on the landing page is a plain `<a href>` in the served HTML, not inserted by script, because a fetching agent reads the HTML as served. The page also says outright, in visible text, where an agent should start (`/llms.txt`).
- Links between docs stay **relative `.md` links**, as they already are, so the same files work on the site, on GitHub, and in the binary's output.

### Decision 3 — The docs remain user docs; the agent is the reader, not the subject

The docs describe how to **install, configure, use and troubleshoot** dot-agent-deck. They are read by the user's agent on the user's behalf. That settles what goes in:

- **Task-oriented.** "Set up a three-role orchestration" rather than a tour of features. Each task gives exact commands and config, and says how to check that each step worked.
- **Complete reference where the agent writes files.** The `.dot-agent-deck.toml` keys, orchestration role definitions, schedule syntax and CLI flags are stated fully and precisely, because the agent will generate them.
- **Internals only when the user's agent needs them to diagnose a problem.** For example: where the log file is, what a status means, why a pane shows `orphaned`. Design rationale, implementation mechanisms and project history do not belong. Where such material exists today and is not already in `docs/develop/`, move it there rather than delete it.
- **Precision matters more.** A person skims past a sentence that says too much; an agent acts on it. Rule 17 (no unverified absolutes) is therefore a correctness requirement for these pages, not only a matter of style.
- **Failure modes stated.** What goes wrong, how it shows up, and what to do. Most of this lives in `troubleshooting.md` today and is linked from the task pages.

### Decision 4 — `dot-agent-deck docs` prints version-matched docs embedded at build time

- `dot-agent-deck docs` lists topics with a one-line description of each. `dot-agent-deck docs <topic>` prints that page's Markdown to stdout, and `dot-agent-deck docs --all` prints every page in manifest order (the CLI counterpart of `llms-full.txt`). An unknown topic exits non-zero and lists the valid ones.
- **Topic names are the manifest slugs, which are the file paths under `docs/` without `.md`** (`orchestration`, `desktop/voice`). That makes the pages' relative links resolvable from stdout, which has no base path: a link to `orchestration.md#section` in a page means `dot-agent-deck docs orchestration`, at that heading, and a link to `../configuration.md` from a desktop page resolves against the page's own directory exactly as it would on disk. The output of `docs <topic>` states this rule in a one-line preamble, so an agent reading it cold knows how to follow a link. The Markdown itself is printed unmodified, so the same bytes serve the site, GitHub and the terminal. A test asserts that every relative link in every embedded page resolves to a manifest topic, and every anchor to a heading in that page.
- The content is embedded at build time (for example with `include_str!` over a generated list, or a `build.rs` step), so the binary never needs the network and always prints the docs for its own version. `build.rs` must emit `rerun-if-changed` for the embedded files.
- The same source files feed the site and the binary; there is one copy of the docs.
- The landing-page prompt and `llms.txt` both mention the subcommand, so an agent that finds the website first learns the version-matched route exists.
- **No experimental flag** (maintainer decision, 2026-09-29). The subcommand is read-only and has nothing to hide.

### Decision 5 — One manifest decides what is published; `docs/develop/` stays out, enforced rather than configured

`docs/published.toml` lists each published page's slug, title and one-line description. It is the single source for three things: what the site publishes, what `llms.txt` lists, and which topics `dot-agent-deck docs` offers. A page not in it is not published anywhere.

Today one Docusaurus `exclude` glob (`develop/**`) is what keeps maintainer docs off the site. That glob disappears with Docusaurus, and a naive "copy `docs/`" would publish all 39 files. The manifest is the replacement, plus a test that fails if anything under `docs/develop/` appears in the site build output, in `llms.txt`/`llms-full.txt`, or among the embedded topics. None of the 20 user pages links into `docs/develop/` today, and the link check keeps it that way.

### Decision 6 — One PRD, one PR, one commit per piece

The maintainer reviews it all at once, so that the landing page, the content, `llms.txt` and the subcommand can be checked for agreement with each other. To keep one large PR reviewable, the branch is structured as one commit per milestone below, reviewable in order.

## Milestones

- [ ] **Site replaced**: Docusaurus removed; the #1155 landing page ported to static HTML/CSS with its own header and footer, light and dark mode, an agent prompt and a plain-HTML docs link; cleaned screenshots in place; Markdown served as `text/markdown` on both nginx and Netlify, every old `/docs` URL form redirected (generated from the manifest and tested against the last Docusaurus build's URL list), and a link check in place of Docusaurus's.
- [ ] **Docs rewritten for the agent reader** per Decision 3: task-oriented, full config/CLI reference, internals only where diagnosis needs them, with removed internals moved to `docs/develop/` where not already there.
- [ ] **Manifest and generation**: `docs/published.toml`, and a `cargo xtask` that builds the site output (Markdown, `llms.txt`, `llms-full.txt`, images, landing page) from it.
- [ ] **`dot-agent-deck docs [topic]` subcommand** printing embedded, version-matched docs, with tests.
- [ ] **Publication boundary enforced**: tests that `docs/develop/` is absent from the build output, the `llms` files and the embedded topics.
- [ ] **Repo rules and skills updated**: CLAUDE.md rule 11, `publish-docs` skill, `CONTRIBUTING.md`, plus a changelog fragment.
- [ ] **Validated with a fresh agent**: in a sandbox, an agent given only the landing-page prompt installs dot-agent-deck and sets up a three-role orchestration without other help. A second run starts from `dot-agent-deck docs` alone. Record both transcripts' outcomes in the PR.

## Validation

- `cargo test-fast` for the subcommand and the publication-boundary tests, plus rule 2's fmt and clippy.
- The site build runs locally and in CI, and the link check passes.
- A manual check that `.md` URLs render as text in a browser and return `text/markdown` (`curl -I`) on both a Netlify deploy preview and the nginx image.
- The fresh-agent validation milestone above. Its outcome, not the build going green, is the acceptance test for the PRD.

## Risks

- **Losing #1155's design in the port.** Mitigated by porting the existing CSS rather than restyling, and comparing old and new pages side by side at desktop and phone widths, in both themes, before the Docusaurus build is deleted.
- **Lost discoverability.** Search engines indexed the HTML doc pages. Mitigated by the redirects and by keeping the Markdown published at stable URLs.
- **Rewrite quality.** An agent following an inaccurate page does damage faster than a person would. Mitigated by Decision 3's precision requirement, review of the rewrite as its own commit, and the fresh-agent validation.
- **Version skew between site and binary.** The site shows the latest docs. Mitigated by the subcommand, which the site points to.
- **Large PR.** Accepted by the maintainer. Mitigated by the one-commit-per-milestone structure.

## Open Questions

- Keep an explicit light/dark toggle, or follow the OS setting only?

## Work Log

- **2026-09-29**: PRD created. Decisions from discussion with the maintainer: landing page for people plus Markdown docs for agents, linked from the landing page; `llms.txt`; version-matched `docs` subcommand; docs remain user-facing and cover internals only where diagnosis needs them; dev docs out of scope; one PRD and one PR with one commit per piece; no experimental flag; work in a worktree.
- **2026-09-29**: Decided with the maintainer: drop Docusaurus and port the #1155 landing page to static HTML/CSS (its only Docusaurus dependencies are the layout wrapper and `Link`); replace front matter with a `docs/published.toml` manifest shared by the site, `llms.txt` and the `docs` subcommand; use the `.landing-assets/` screenshots after cleaning (busy deck on the landing page, dispatch in the dispatcher-mode doc).
- **2026-09-29**: Review of PR #1421 (Greptile). Specified how relative links in `dot-agent-deck docs` output resolve (topics are file-path slugs, a stated preamble rule, a resolution test, plus `--all`), and widened the redirect requirement to every URL form Docusaurus serves (slashless, trailing slash, nested desktop pages, the `/docs/desktop` category index, `/docs`). Corrected the page count from 15 to 20: the first draft was counted from a stale checkout that predated `docs/desktop/`.
