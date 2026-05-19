# PRD #87: Remote Environments Documentation

**Status**: Planning
**Priority**: Medium
**Created**: 2026-05-16
**GitHub Issue**: [#87](https://github.com/vfarcic/dot-agent-deck/issues/87)
**Depends on**: PRD #76 (Remote Agent Environments) being shipped first.

## Problem Statement

PRD #76 ships the code for remote agent environments (ssh-`-t` wrapper, daemon-as-separate-process, TUI-on-remote, persistent agent registry). The user-facing documentation for that feature was deliberately **kept out of the published docs site** while the code path was being stabilized.

Three concrete consequences as it stands:

1. **Users have no published guide for the remote feature.** `dot-agent-deck remote add` and `dot-agent-deck connect` exist in the binary but are absent from the docs sidebar. A new user reading the docs site does not learn the remote workflow exists.
2. **Three drafted pages live in `docs/` but are not surfaced.** `docs/remote-environments.md`, `docs/remote-recipes.md`, and `docs/remote-requirements.md` were written under PRD #76 and remain on disk as the working draft, but `site/sidebars.js` does not include them so the production site does not link to them.
3. **The pre-existing pages were intentionally left unmodified.** `docs/getting-started.mdx` and `docs/installation.md` were restored to their pre-PRD-76 state. They currently make no reference to the remote feature.

PRD #76's Phase 5 originally bundled documentation with the code release. The work was split out into this PRD so the docs can land once the code has been validated end-to-end (manual VM testing under PRD #76's M2.10 / M4.4).

## Solution Overview

Take the three drafted pages from disk, polish them against the shipped code path, add them to the sidebar, and add the right cross-references in the pre-existing pages. Cut a release with a changelog fragment that explains the new feature for users.

The work is documentation-only. No Rust source files in this PRD's scope. No protocol or behavior changes.

## Scope

### In Scope

- **Polish the three drafted pages** (`docs/remote-environments.md`, `docs/remote-recipes.md`, `docs/remote-requirements.md`) to match the shipped code path. The drafts already describe the TUI-on-remote pivot, but they were written against in-flight code — anything that drifted before PRD #76 closed should be reconciled.
- **Add the pages to `site/sidebars.js`** under appropriate positions (e.g. requirements → recipes → environments in the user-journey order, slotted near `configuration` or after `workspace-modes`).
- **Cross-reference from `docs/getting-started.mdx`** — short "Running on a remote host" section pointing readers at the three new pages.
- **Cross-reference from `docs/installation.md`** — one paragraph noting that remote-host installs are handled automatically by `remote add`.
- **Screenshots, if useful** — the docs site uses annotated screenshots (see PRD #51). The remote pages may benefit from one or two (e.g. `remote list` output, dashboard on the remote, the quit/detach dialog).
- **Changelog fragment** — drafted via `dot-ai-changelog-fragment` summarizing the remote feature for end users.
- **Release** — tag a new version once the docs land. Cadence and tag are the maintainer's call.

### Out of Scope

- Anything that requires a code change in `src/`. If polishing the drafted pages surfaces a doc-vs-code drift, file a follow-up issue or a tiny code patch under a different PRD; do not bundle it here.
- Marketing copy, blog posts, or video walkthroughs. Reference docs only.
- Translation. The docs site is English-only.
- New provisioning recipes beyond what `docs/remote-recipes.md` already drafts. Adding a recipe per cloud provider is a maintenance treadmill the project has explicitly rejected.

## Success Criteria

- A user reading the docs site from scratch can discover the remote feature without leaving the published sidebar — i.e., `remote-environments`, `remote-requirements`, and `remote-recipes` all appear in `site/sidebars.js`.
- The published pages match the binary's actual behavior. `dot-agent-deck connect`, `dot-agent-deck remote add`, the quit/detach dialog, and the failure-mode error messages described in the docs match what the running deck prints.
- `docs/getting-started.mdx` mentions the remote workflow in one short section with a link to `remote-environments.md`.
- `docs/installation.md` mentions that remote installs are automatic via `remote add`.
- A user-readable changelog fragment is drafted for the release that lands these docs.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` continue to pass (sanity check; nothing in this PRD should touch Rust, but the gate stays on).

## Milestones

### Phase 1: Polish

- [ ] **M1.1** — Read the three drafted pages against the shipped `src/connect.rs`, `src/remote.rs`, `src/daemon.rs`, and `src/ui.rs` quit-dialog code. Note every divergence (verbatim error strings, command flags, env var names, default paths). Fix the docs.
- [ ] **M1.2** — Reconcile `docs/remote-environments.md`'s lifecycle table with the real `Ctrl+W` / quit-dialog / ssh-disconnect behavior, including any local-mode quit-dialog change tracked in Task #20 (hide Detach in local mode).

### Phase 2: Publication

- [ ] **M2.1** — Add `remote-requirements`, `remote-recipes`, and `remote-environments` to `site/sidebars.js` in user-journey order.
- [ ] **M2.2** — Add the "Running on a remote host" section to `docs/getting-started.mdx`.
- [ ] **M2.3** — Add the remote-install note to `docs/installation.md`.
- [ ] **M2.4** — Local preview build (`cd site && npm run start` or equivalent), confirm pages render with no broken links and the sidebar position feels right.

### Phase 3: Screenshots (optional)

- [ ] **M3.1** — One screenshot of `dot-agent-deck remote list` output in a terminal.
- [ ] **M3.2** — One screenshot of the dashboard running on a remote, viewed via `connect`.
- [ ] **M3.3** — One screenshot of the quit/detach dialog.

(Skip this phase if the prose is already clear without screenshots.)

### Phase 4: Release

- [ ] **M4.1** — Draft a changelog fragment via the `dot-ai-changelog-fragment` skill.
- [ ] **M4.2** — Open a PR. Reviewer + auditor pass per the project's standing workflow.
- [ ] **M4.3** — Merge and cut a release (`dot-ai-tag-release`).

## Key Files

- `docs/remote-environments.md` — drafted under PRD #76; polish + publish here.
- `docs/remote-recipes.md` — drafted under PRD #76; polish + publish here.
- `docs/remote-requirements.md` — drafted under PRD #76; polish + publish here.
- `docs/getting-started.mdx` — add a remote cross-reference section.
- `docs/installation.md` — add a remote-install note.
- `site/sidebars.js` — add the three new entries.
- `changelog.d/<fragment>.md` — drafted via the changelog skill.

## Design Decisions

### 2026-05-16: Split docs into a separate PRD from the code

PRD #76 originally had Phase 5 bundle docs with the code release. The decision to split was made after the code reached a stable point but before end-to-end VM validation. Rationale: doc polish wants the binary's actual error strings and quit-dialog text settled; releasing both together would either delay the code or ship docs that drift. Splitting lets PRD #76 close on the code, and this PRD picks up once the user has signed off on the runtime behavior.

The drafted doc pages remain on disk in PRD #76's branch so this PRD has a working starting point, but they are explicitly **not** in the sidebar — the published docs site is unchanged by PRD #76.
