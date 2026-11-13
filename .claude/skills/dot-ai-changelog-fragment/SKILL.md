---
name: dot-ai-changelog-fragment
description: Decide whether a change needs a changelog fragment for the release notes and, if it does, write it. Invoke from /pr-create's "Before the PR" step, before the branch is pushed, or whenever a finished change is about to be committed for a PR.
user-invocable: true
---

# Create Changelog Fragment

Write the towncrier fragment the release notes are built from, in the same PR as the change so it is reviewed with it.

## Workflow

### Step 1: Ask whether a fragment exists at all

[CLAUDE.md rule 19](../../../CLAUDE.md) decides this: a fragment exists when a **user can observe the change**. CI, developer tooling, the test harness, a refactor with no behavioural difference and repository configuration get **no fragment, of any type** — stop here and say so. The exception is a semantic contract break (CLAUDE.md rule 12): it gets a `.breaking.md` fragment whatever a user can see, plus its `CONTRACT_BREAKS` entry in `src/daemon_protocol.rs`. When in doubt between internal and user-observable, write the fragment.

### Step 2: Read the change

The source is the change itself, not a particular document:

- the **issue** the branch works (`gh issue view <n>`; the number is usually in the branch name or the task),
- for PRD work, the **PRD** — its issue body (issue #1591) or, for an older PRD, its `prds/` file,
- the **diff** (`git diff origin/main...HEAD`), which is what actually shipped, and the user docs it touched.

Extract what the user can now do or no longer suffers, how they use or configure it (commands, keys, flags, config fields, environment variables), what they see in both the TUI and the desktop app (CLAUDE.md rule 22), and which user docs page covers it.

### Step 3: Determine the fragment type

Read `pyproject.toml`. It is the authority on the types: each `[[tool.towncrier.type]]` section has a comment above it describing when to use that type, and a `directory` field that is the type identifier in the filename (e.g. `feature` for `.feature.md`).

**Ask the breaking question first.** Read the `breaking` comment: here it means the TUI↔daemon contract, including a same-wire change whose meaning shifts (CLAUDE.md rule 12), not generic user-facing breakage. When the change matches, or plausibly could, use `breaking`: under-classifying ships the wrong version bump. Otherwise pick the type whose comment matches. A change can carry more than one fragment (`1885.feature.md` and `1885.breaking.md`).

### Step 4: Write the fragment

Create `changelog.d/<issue>.<type>.md` — a flat file, not a subdirectory (`changelog.d/329.feature.md`, never `changelog.d/feature/329.md`). `<issue>` is the issue number the change closes.

Content: an optional `## Title` heading, then a paragraph saying what the user can now do and why it matters, then how they use or configure it, then a link to the docs page if one covers it. CLAUDE.md rule 21 applies: what the user does, sees and configures; no internal mechanisms or issue history.

Link docs as `https://agent-deck.devopstoolkit.ai/docs/<slug>.md`, where `<slug>` is the page's path under `docs/` without `.md`, as listed in `docs/published.toml` (CLAUDE.md rule 11). Never link a `docs/develop/` page.

**Example** (`changelog.d/1958.feature.md`, shortened):

```markdown
## Close a dispatched unit and keep its worktree

You can now stop a dispatched unit, or a whole orchestration, and keep its worktree on disk, clean or not, so you can start it again in the same directory later.

In the TUI, the `Ctrl+W` close confirmation offers **Close, keep worktree** for a dispatched unit. In the desktop app, the agent and orchestration close confirmations offer **Close agent, keep worktree**. From a shell, add `--keep-worktree` to `dot-agent-deck close`.

See [Closing Agents → Keep a worktree on purpose](https://agent-deck.devopstoolkit.ai/docs/closing-agents.md#keep-a-worktree-on-purpose).
```

### Step 5: Confirm

Show the fragment's path and content, and commit it with the change.

## Guidelines

- **User-focused**: what users gain, not how it was implemented.
- **Specific and complete**: concrete examples, every major capability, not only the headline.
- **Present tense, not diary style**: "You can now…" or "The status line now reads…", not "Added support for…".
- **Include configuration**: the commands, keys, flags or settings a user needs.
- **Link to docs** on `agent-deck.devopstoolkit.ai` when a published page covers the change.
