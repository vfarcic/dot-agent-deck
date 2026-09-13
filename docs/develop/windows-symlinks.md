# The tracked symlinks, and what a Windows clone does to them

## What the repo tracks

Three paths in this repository are committed as symlinks (git mode `120000`), one per family:

| Path | Target | What it is for |
|---|---|---|
| `AGENTS.md` | `CLAUDE.md` | The file Codex and OpenCode read for the project's conventions |
| `.agents/skills` | `../.claude/skills` | The same skills tree Claude gets, exposed to Codex/OpenCode |
| `docs/img` | `../site/static/img` | The docs' image directory, shared with the Docusaurus site |

The first two exist so that **one** `CLAUDE.md` and **one** skills directory reach every agent the deck supports, instead of three near-copies drifting apart. That is a deliberately good design, and it is worth keeping — the whole point of this page is what it costs on one platform and how that cost is contained.

## What Windows does to them

Git only creates symlinks on checkout when `core.symlinks` is on. On Windows it is off unless the clone had the privilege to create links — which means Developer Mode is enabled (Settings → System → For developers), or git ran elevated, or the Git for Windows installer's "Enable symbolic links" option was ticked (it writes `core.symlinks=true` into the system config).

With it off, git does not fail and does not warn. It writes each symlink as **a plain text file whose entire content is the link target**:

- `AGENTS.md` becomes a **9-byte file containing the string `CLAUDE.md`**. Codex and OpenCode read it, find nine bytes of nothing in particular, and proceed with **no project instructions at all**.
- The `.agents/skills` link becomes a one-line text file, so the whole skills tree resolves to nothing.
- `docs/img` stops being a directory, so anything resolving image paths through it breaks.

The failure mode that matters is not the breakage, it is the **silence**. Every file exists and is readable. An agent running with no instructions is indistinguishable, from the outside, from a correctly configured agent that happens to have nothing to follow — so the symptom shows up later as an agent that ignores conventions nobody can see it was never given.

## Before you clone on Windows

Do one of these first — the setting is consulted at checkout time, so turning it on later does not convert the files already on disk; they have to be checked out again (see below):

1. Turn on **Developer Mode** (Settings → System → For developers). That is what lets an *unelevated* process create symlinks at all, which is what an ordinary `git clone` is.
2. And/or set the config explicitly: `git config --global core.symlinks true`.

To repair a clone that already came out wrong, turn the setting on and check the tree out again:

```sh
git config core.symlinks true
git checkout -- .
```

The second command is enough on its own once the first is in place — no need to delete the placeholder files by hand. With `core.symlinks` on, git sees a regular file where the index says `120000`, treats it as modified, and rewrites it as a link.

Verified on `windows-latest`, not inferred from Linux: a clone made deliberately with `git clone -c core.symlinks=false` produced a 9-byte `AGENTS.md`, the checker below rejected every one of them, and after these two commands `AGENTS.md` was a symlink resolving to 29,875 bytes of `CLAUDE.md` with the checker reporting them all materialised. (That run measured 35 links, because `.agents/skills` was then 33 per-skill entries rather than the single directory link it is now; the check is count-agnostic, so the result stands.)

## The check

```sh
scripts/check-symlinks.sh              # check this working tree
scripts/check-symlinks.sh <dir>        # check a tree materialised elsewhere
scripts/check-symlinks.sh --self-test  # prove the check can actually fail
```

For every tracked symlink it reads the link target out of the blob (a symlink's blob *is* its target) and asserts that the path on disk resolves to that target: a directory target must resolve to *that* directory (`cd` into both sides and compare `pwd -P`), and a file target must compare equal to the file. Asserting identity rather than shape matters for the directory half — checking only that `.agents/skills/<name>` *is a directory* would pass a link redirected at some other existing directory, so the entire skills tree could point somewhere else and the check would still be green. It prints every offender rather than stopping at the first, and it names the failure it found — the Windows fallback is reported as `did not materialise: it is a 9-byte plain file whose content is the literal text "CLAUDE.md"`, which is meant to be readable by someone who has never heard of `core.symlinks`.

Three details are deliberate:

- **It compares what the path resolves to, not link-ness.** `test -L` on a native Windows symlink answers a question about the shell doing the asking, not about the checkout — Git Bash's MSYS layer resolves them, another shell may not. What actually matters is that an agent reading `AGENTS.md` gets `CLAUDE.md`'s content, and comparing against the target tests exactly that. It also catches two cases `-L` would miss: a stale hand-made copy that is a real file with the right name and the wrong content, and a link quietly redirected at a different directory.
- **It refuses to pass on an empty set.** Zero tracked symlinks would otherwise read as success both when the repo genuinely has none and when git reported nothing (wrong directory, unreadable index) — and the second reading turns the whole check into a green light that means nothing. Zero exits 2.
- **It carries its own negative control.** `--self-test` writes the index into a temp directory with `core.symlinks=false` — the exact state a Windows clone lands in — and asserts that the checker rejects that tree *for the materialisation reason specifically*, not merely that it exits non-zero. This is what keeps a green result honest, and it works on any platform: `core.symlinks` is a git config, not a Windows feature, so Linux and macOS can reproduce the Windows breakage faithfully and locally.

That the check discriminates was confirmed on Windows itself and not only through the self-test's stand-in. On a `windows-latest` runner, a deliberately broken clone (`git clone -c core.symlinks=false`) gave the real thing — a 9-byte `AGENTS.md` written by real git on a real NTFS working tree — and the checker exited 1 naming every link, while the same script on the same runner exited 0 against the job's own checkout seconds earlier. Both cases, one machine, opposite results.

Both need nothing but git and coreutils. On Linux the pair runs in ~0.45s; on the Windows runner it is 7–11s across runs, two thirds of that the self-test writing the whole index out — Windows process spawn dominates, and it is still an order of magnitude cheaper than anything else in that job.

## Where it runs, and what CI can and cannot catch

The check runs in the `build-windows` job in `.github/workflows/ci.yml`, immediately after the checkout and before any toolchain step, as `--self-test` followed by the real check. Windows is the only platform where it can fail for the reason it exists; a Linux or macOS leg would only re-prove that Unix creates symlinks.

**It is a regression guard, not a discovery mechanism — and the distinction is measured, not assumed.** A probe run on `windows-latest` (image `windows-2025-vs2026`, git `2.55.0.windows.3`) found `core.symlinks=true` already set in the **system** gitconfig at `C:/Program Files/Git/etc/gitconfig`, so `actions/checkout` produces real links there: `AGENTS.md` came out as a symlink resolving to 29,875 bytes of `CLAUDE.md`, and `.agents/skills/demo-reel` came out as a directory. The step is therefore green from its first run, and what it defends is a *future* change to the runner image, to `actions/checkout`, or to the repo's own set of tracked links — including a link whose target has been deleted, which the same check reports as dangling.

What it structurally cannot see is the case the issue was actually about: a **contributor's own clone**, on their own machine, with symlinks off. No CI job can observe that, because CI never sees their working tree. That half is covered by the setup note in [`CONTRIBUTING.md`](../../CONTRIBUTING.md) and by running the script above, which is why both exist rather than either alone.

## Why the symlinks stay

The alternative is to stop relying on a symlink for `AGENTS.md` — copy it, or generate it. That was considered and rejected:

- It fixes **one** of the three. `.agents/skills` and `docs/img` are directory links; duplicating a whole skills tree is worse than duplicating one file, so the Windows exposure would remain for the larger part of the set while the cleanest part of the design was given up.
- A copy can go stale, and a copy that goes stale fails the *same silent way* — an agent reading conventions that no longer match. Preventing that means a generator plus a CI drift check, which is more machinery than the check this page describes, guarding a weaker property.
- The blast radius is small. **No Windows binaries are released** — `release.yml` builds `x86_64`/`aarch64` for `apple-darwin` and `unknown-linux-gnu` only — so no end user is exposed. The population is Windows *contributors* building from source, for whom a one-time setting is a proportionate fix.

If the balance ever changes — a released Windows artifact, or contributors repeatedly losing time to this — revisit it then, with `.agents/skills/` in scope rather than `AGENTS.md` alone.
