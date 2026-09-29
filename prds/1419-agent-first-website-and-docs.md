# PRD #1419: Agent-first website and docs

**Status**: Draft — not started
**Priority**: Medium
**Created**: 2026-09-29
**Issue**: [#1419](https://github.com/vfarcic/dot-agent-deck/issues/1419)

## Problem Statement

Everyone who uses dot-agent-deck is, by definition, already running an AI coding agent. The website at `agent-deck.devopstoolkit.ai` does not use that fact. It is a Docusaurus site of 15 pages (about 3,800 lines) written for a person to read and navigate, and most of what those pages describe (writing `.dot-agent-deck.toml`, defining orchestration roles, setting up remote environments, scheduling tasks) is work a user would rather hand to the agent sitting next to the deck.

The agent cannot pick that work up reliably today, for two reasons:

- **It does not know the project exists.** dot-agent-deck is niche and newer than most models' training data. Asked "set up dot-agent-deck orchestration for me", an agent either guesses or searches, and the site it finds is laid out for human navigation (JavaScript-rendered pages, sidebars, theme chrome) rather than for reading.
- **The docs it could find may not match the installed version.** The site tracks the latest release, while a user's binary may be several releases older. A person reading a stale page notices; an agent executes it literally.

## Solution Overview

Make the website a landing page for people and make the docs a reference for the user's agent.

1. **A single landing page** for people: what the tool is, why it is useful, a short demo, the one-line install, and a copy-paste prompt that hands the user's agent a concrete starting point ("Read https://agent-deck.devopstoolkit.ai/llms.txt, then help me install and set up dot-agent-deck").
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

- Replace the Docusaurus site with a landing page plus published Markdown (Decision 1).
- Serve `.md` with a readable content type on **both** deploy targets `docs-publish.yml` feeds: the nginx image (`site/nginx-default.conf`, rolled out by Argo CD from `site/helm/`) and Netlify (`site/netlify.toml`).
- `llms.txt` / `llms-full.txt` generation, listing every published page and nothing from `docs/develop/`.
- `dot-agent-deck docs` (list topics) and `dot-agent-deck docs <topic>` (print one page), with the content embedded at build time.
- Rewrite the 15 user pages for the agent reader, per Decision 3.
- Redirects from the old `/docs/<page>/` URLs to their Markdown successors, so existing inbound links (the README, past changelogs and release notes, search results) keep resolving.
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

### Decision 1 — Replace Docusaurus with a static landing page plus Markdown files (proposed; confirm at start)

With the docs served as Markdown, Docusaurus's only remaining job would be rendering one page. A static HTML landing page, the Markdown files copied verbatim, `llms.txt`/`llms-full.txt`, and images is a much smaller build. The delivery pipeline stays: the same `site/Dockerfile` → GHCR → `site/helm` → Argo CD path, the same Netlify deploy, the same `/publish-docs` skill. Only what the build produces changes.

Consequences to handle:
- All 15 pages carry Docusaurus front matter (`sidebar_position`, `title`). Either strip it or keep a minimal `title` that the `llms.txt` generator reads. The published files should not carry a sidebar position that means nothing without a sidebar.
- `docs/img` is a symlink to `../site/static/img`. The published Markdown and the embedded copy must resolve images the same way, or the embedded copy must drop them.
- The existing landing page is a 406-line React component (`site/src/pages/index.js`). It becomes the source material for the new page, not the page itself.

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

- `dot-agent-deck docs` lists topics with a one-line description of each. `dot-agent-deck docs <topic>` prints that page's Markdown to stdout. An unknown topic exits non-zero and lists the valid ones.
- The content is embedded at build time (for example with `include_str!` over a generated list, or a `build.rs` step), so the binary never needs the network and always prints the docs for its own version. `build.rs` must emit `rerun-if-changed` for the embedded files.
- The same source files feed the site and the binary; there is one copy of the docs.
- The landing-page prompt and `llms.txt` both mention the subcommand, so an agent that finds the website first learns the version-matched route exists.
- **No experimental flag** (maintainer decision, 2026-09-29). The subcommand is read-only and has nothing to hide.

### Decision 5 — `docs/develop/` stays unpublished and unembedded, enforced rather than configured

Today one Docusaurus `exclude` glob (`develop/**`) is what keeps maintainer docs off the site. That glob disappears with Docusaurus, and a naive "copy `docs/`" would publish all 39 files. The replacement is an **explicit list of published pages** (the same list that drives `llms.txt` and the binary's topics), plus a test that fails if anything under `docs/develop/` appears in the site build output, in `llms.txt`/`llms-full.txt`, or among the embedded topics. None of the 15 user pages links into `docs/develop/` today, and the link check keeps it that way.

### Decision 6 — One PRD, one PR, one commit per piece

The maintainer reviews it all at once, so that the landing page, the content, `llms.txt` and the subcommand can be checked for agreement with each other. To keep one large PR reviewable, the branch is structured as one commit per milestone below, reviewable in order.

## Milestones

- [ ] **Site replaced**: static landing page (pitch, demo, install, agent prompt, plain-HTML docs link), Markdown served as `text/markdown` on both nginx and Netlify, old `/docs/<page>/` URLs redirected, and a link check in place of Docusaurus's.
- [ ] **Docs rewritten for the agent reader** per Decision 3: task-oriented, full config/CLI reference, internals only where diagnosis needs them, with removed internals moved to `docs/develop/` where not already there.
- [ ] **`llms.txt` / `llms-full.txt` generated** from the explicit published-page list.
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

- **Lost discoverability.** Search engines indexed the HTML doc pages. Mitigated by the redirects and by keeping the Markdown published at stable URLs.
- **Rewrite quality.** An agent following an inaccurate page does damage faster than a person would. Mitigated by Decision 3's precision requirement, review of the rewrite as its own commit, and the fresh-agent validation.
- **Version skew between site and binary.** The site shows the latest docs. Mitigated by the subcommand, which the site points to.
- **Large PR.** Accepted by the maintainer. Mitigated by the one-commit-per-milestone structure.

## Open Questions

- Confirm Decision 1 (drop Docusaurus entirely) at the start of implementation.
- Should the published Markdown carry any front matter (for example `title`, `description`) for the `llms.txt` generator, or should the generator read the first heading?
- Is the untracked `.landing-assets/` directory in the main checkout (two PNGs) meant as input for the new landing page?

## Work Log

- **2026-09-29**: PRD created. Decisions from discussion with the maintainer: landing page for people plus Markdown docs for agents, linked from the landing page; `llms.txt`; version-matched `docs` subcommand; docs remain user-facing and cover internals only where diagnosis needs them; dev docs out of scope; one PRD and one PR with one commit per piece; no experimental flag; work in a worktree.
