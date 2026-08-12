# Maintainers

Maintainers may approve and merge pull requests. There is no per-area ownership: a maintainer reviews anywhere in the tree.

| Maintainer | GitHub | Role |
| --- | --- | --- |
| Viktor Farcic | [@vfarcic](https://github.com/vfarcic) | Owner |
| Prageeth Warnak | [@prageethw](https://github.com/prageethw) | Maintainer |

## How this list is enforced

It isn't, directly — it documents the GitHub collaborator list, which is the actual mechanism.

`main` requires one approving review, and GitHub only counts approvals from accounts with **write** or **admin** permission. So the set of people who can satisfy that requirement *is* the collaborator list. This file exists so that set is visible in the repository rather than only in repository settings, and it must be updated in the same change that grants or revokes access.

[`.github/CODEOWNERS`](.github/CODEOWNERS) mirrors this table as a single pathless rule, `* @vfarcic @prageethw`. GitHub omits the PR author when auto-requesting review from code owners, so that one line routes every pull request to the *other* maintainer automatically. Update it in the same change as the table above. It is deliberately pathless — per-path ownership is the part that goes stale silently when files are renamed or split — and it stays a router rather than a gate, because the ruleset keeps `require_code_owner_review: false` and any maintainer's approval satisfies the requirement.

See [`docs/develop/governance.md`](docs/develop/governance.md) for the ruleset itself, how review is requested, and the rollout sequence.

## Becoming a maintainer

The owner invites maintainers. There is no fixed contribution threshold, but the question asked is whether someone's judgement about the codebase is one the existing maintainers would accept when it contradicts their own — because that, not merge access, is what the role actually confers.

Being a maintainer is recurring work: you are a required reviewer for the other maintainers' pull requests, including on the daemon, the TUI↔daemon protocol, orchestration, and hooks, where a mistake breaks interoperability between an older and a newer build (see CLAUDE.md rule 12).
