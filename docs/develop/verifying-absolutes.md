# Verifying absolutes: the evidence behind CLAUDE.md rule 17

CLAUDE.md rule 17 is the policy: verify an absolute claim about how the system behaves before writing it, or write the narrower claim that is true. This page is the evidence that made it a rule, and the sweep technique it prescribes, moved out of CLAUDE.md by issue #905. "Rule 13" below means CLAUDE.md rule 13.

## The over-claims that shipped

**The evidence is thirteen audit rounds on PR #805** (issues #502, #785). Every row below shipped and had to be narrowed:

| claim as written | what was true |
| --- | --- |
| "no agent credential reaches CI" | the Codex issue-labeler holds `OPENAI_API_KEY` on a runner |
| "NO SECRETS IN THIS JOB" (a `ci.yml` comment) | `actions/checkout` brings the automatic `GITHUB_TOKEN` and persists it into `.git/config` |
| "NOTHING ELSE DOES EITHER" / "the ONLY place anyone will" | the `test-e2e-live` alias and `bacon test-e2e-live`, locally |
| "the union can ONLY add redactions" | non-monotonic under a known matcher gap — enlarging the set can *reduce* protection |
| "a recording can NO LONGER be stale" | two residual routes |
| "NO credential worth protecting is shorter" than the 16-byte floor | `REDISCLI_AUTH` |
| "`skip_unless!` is the first line of ALL of them" | a majority call it, and not always as the first statement |
| "exercised by EVERY one of the `tests/e2e_*.rs` files" | the ones that launch a deck |

Each row is the same shape — a narrower fact written one quantifier too wide — which is why this is a *grammatical* defect class and survives review aimed at subject matter.

## Why the sweep is for the construction, not the topic

**So sweep for the construction, not the topic.** Sweeps aimed at subjects — the containment story's `env_clear` paths (`590a645`), then present-tense CI claims (`de8d5ca`) — each missed the next instance, and `65a873e` was confident enough to be titled "narrow the **last** false CI-credential absolute" before three later commits narrowed more of the same shape, one of them another CI-credential absolute in `ci.yml`. Enumerating the constructions instead and classifying every hit produced **26** edit sites across 14 files in a single commit: `git show -U0 --format='' 193f99c | grep -c '^@@'`.

## Why the grep needs word boundaries

**Grep with word boundaries, and blank inline code spans first** — the inverse trap cost a round of its own. A case-insensitive search for `actions` in `tests/common/mod.rs` matched 110 lines on PR #805's branch, and all 132 occurrences sat inside a `…redactions…` identifier; `\bActions\b` matched none of them. A sweep that drowns in substring noise gets abandoned, which buys exactly as much as not sweeping at all.
