# E2E temp directories

How the test harness allocates scratch space, why it used to leak, and what to run when a machine has accumulated leftovers. Background: [issue #322](https://github.com/vfarcic/dot-agent-deck/issues/322).

## The one-root rule

Every temp directory allocated by a test process **that links the harness** nests under a single per-process root named `dad-tests-<pid>-<random>`, created on first use in the [temp base](#where-the-root-lives) — by default `/var/tmp/dad-e2e-<uid>`. The e2e suite allocates through `common::harness_tempdir()` / `common::harness_tempfile()` (and `race_safe_tempdir()`, and the daemon lock dir), each of which resolves the root *before* it allocates, so containment holds no matter what order a test does things in. So there is exactly one place to look and exactly one thing to clean up.

Test processes that do **not** link the harness are covered separately and more weakly — see [what is outside the root](#what-is-outside-the-root) for the exact boundary, which is measured rather than assumed.

`harness_temp_root()` *also* points the `tempfile` crate's own process-global default directory at the root (`tempfile::env::override_temp_dir`), but that is **defence in depth and nothing more** — it catches allocations this repo does not make itself, once the root exists. It is not what contains the suite, and treating it as such is a mistake worth naming: the override is installed at the end of the root's lazy initialiser, so it is in force only from the first moment something asks the harness for a directory. A bare `tempfile::tempdir()` running before that is the process's **first** allocation and goes to the OS temp dir instead — measured on `a0b616c`, `/tmp/.tmpz5pszS` while the root was `/var/tmp/dad-e2e-1000/dad-tests-…`, in every fast-tier binary. nextest runs one process per test, so "before any harness call" is an ordinary thing for a test body to be. `linkage-check` rule 8 is what keeps bare constructors out of the covered files; it is a mechanical check because the ordering argument is invisible in a diff.

Rule 8 matches **file** constructors as well as directory ones — `NamedTempFile::new()`, `tempfile::tempfile()` and the builder's `.tempfile()` alongside `tempdir()` / `TempDir::new()`. It did not always: the Codex-auth pre-flight in `tests/common/mod.rs` created a `NamedTempFile` in the OS temp dir, inside the rule's own scope, and the rule could not see it. Measured on `5e8e0ed` as four zero-byte `/tmp/.tmp*` files. Zero bytes is not why it was fixed — a guard with a hole in the territory it claims to cover makes the claim on this page false, which is the one unacceptable outcome. The `…_in(parent)` forms name their destination explicitly and are deliberately not matched; they are what the wrappers themselves call.

## What is outside the root

This section is the claim. Everything in it was measured with live `/tmp` sampling during a recorded `cargo test-e2e`, not reasoned about.

**Processes the harness spawns** — agents, daemons, `git`, `gh` — resolve temp the way the OS tells them to, which is `TMPDIR`, unchanged. The redirect is `tempfile`'s own process-global default, not an environment variable, precisely so it does not silently reach into every child. This is by design and is not going to change.

**Test targets that do not link `tests/common/`** cannot reach the root at all — there is no per-process root, no pre-flight and no exit hook in those processes. Three groups, with different answers:

| Where | Contained? | How |
|---|---|---|
| Everything under `tests/` | yes, weakly | `test_temp::tempdir()`, reached by `#[path]`-including `src/test_temp.rs` — the same private base, none of the machinery |
| `src/dispatch.rs` unit tests | yes, weakly | `crate::test_temp::tempdir()` — the same module, reached as an ordinary `crate::` path |
| The rest of `src/`'s unit tests | **no** | bare `tempfile::tempdir()`, OS temp dir |

`src/test_temp.rs` is a ~40-line resolver, not a second harness. It resolves *only* the default `/var/tmp/dad-e2e-<uid>` base (it does **not** read `DAD_E2E_TMPDIR` — a second, weaker reading of a security-sensitive variable is worse than not honouring it), judges that directory by name with `symlink_metadata` rather than the [descriptor walk](#what-dad_e2e_tmpdir-is-checked-for), and on refusal **warns and falls back to the OS temp dir** instead of stopping. The harness stops because it seeds real agent credentials; nothing routed through `test_temp` does, and the fallback is exactly what those call sites did before the module existed, so it cannot be a regression. Its directories are named `dad-unit-*` — not `dad-tests-*`, because they are not process roots and the [forensic signature](#reading-a-leftover-0775-means-the-sweep-worked) does not apply to them — and that name is on the reaper's owned-prefix list, which is the only reason a SIGKILLed leftover under the private base is reclaimable at all.

The first files to route through it were singled out because measurement singled them out. `src/dispatch.rs`'s e2e-gated dispatch test was observed holding a live 184 KiB `/tmp/.tmpYN3lNF` containing a cloned repo and its worktree — small today, but a repo clone is not structurally bounded. `tests/daemon_protocol.rs` was the one file then covered in the fast-tier group that **binds Unix domain sockets**, and `cargo test-e2e` runs the fast tier too, so it was allocating in `/tmp` during the e2e tier; measured as `/tmp/.tmpVtiW6e/attach.sock`, 0 blocks.

The other six files under `tests/` were excluded for a while, and the reasoning is worth recording because it was a measured decision that then expired. Measured on `ebbcf7f` across a full recorded `cargo test-e2e` — 4,028 samples at 50 ms, plus a 20 ms pass that caught nine the first had missed — those six produced **49 unique transient direct `/tmp/.tmp*` directories**. Live `lsof` attributed every one of them to `tests/pane_close.rs` (a `daemon.sock`, LISTENing and in most cases with connected peers) and `tests/rehydration.rs` (an `attach.sock`). Each was a directory at uid/gid 1000, mode 0775, 60 bytes apparent size and `du -sb` zero once the socket went, and each was gone by the end of the run — the same order of magnitude as the earlier ~184 KiB figure, which is to say negligible. The byte count was never the reason to fix them.

What changed is the **price**, not the size. The exclusion was costed at pulling `tests/common/mod.rs` into six more binaries, duplicating its ~530 fast-tier executions to contain small L1 `TempDir`s. `src/test_temp.rs` is deliberately self-contained, so a fast-tier crate can `#[path]`-include it instead: measured with `cargo nextest list` before and after, the fast tier went from **2,315 to 2,327** executions — two per crate, twelve in total, not 530. Two properties also make these allocations worse than 60 bytes suggests. They bind **Unix domain sockets**, which is the shape `tests/daemon_protocol.rs` was promoted for. And on SIGKILL they survive as untagged `.tmp*` — a name [the reaper](#reaping-leftovers) deliberately will not remove by default — so they accumulate rather than clear: the pre-run snapshot for that same measurement held **80** of them. After the conversion, live sampling of `/tmp` across all six binaries shows **zero** new entries, against **52** distinct `dad-unit-*` directories observed under the private base during the same run, all of them reclaimed by the end.

**The one remaining gap is the rest of `src/`'s unit tests** — roughly 82 bare constructors across 22 files, everything in `src/` except `dispatch.rs` and `test_temp.rs` itself. They stay uncovered deliberately: a large mechanical diff, no measured leak behind it, and it would move fast-tier churn onto `/var/tmp` for no benefit anyone has demonstrated. Everything under `tests/` is now contained, and `linkage-check` rule 8 scopes by **directory** rather than by an enumerated list of files, so a *new* file under `tests/` inherits the rule instead of silently falling outside it — which an enumeration cannot do.

The root is removed by an `atexit(3)` hook when the test process exits normally. The hook retries a few times before giving up, because a daemon or agent the test spawned can outlive the test body for a moment and keep writing into the tree, making the first sweep lose a race. If it still cannot remove the root it says so on stderr rather than failing silently.

A process that is **SIGKILLed** never reaches the hook at all and leaves its root behind. That is the one remaining leak path and it is what the reaper below exists for. Measured on a full `cargo test-e2e` run of 3,347 tests: **16 roots totalling ~360 MB**, down from 46 before the retry was added. Running the same tests in isolation leaks nothing, so this is a symptom of parallel load, related to the contention described in [#351](https://github.com/vfarcic/dot-agent-deck/issues/351).

Two properties matter and are easy to break if you touch this code:

- **The root must be created through the single choke point.** `harness_temp_root()` is where the pre-flight space check runs, where the root is created 0o700, and where the `tempfile` redirect is installed.
- **The name must stay distinctive.** The `tempfile` crate's *default* prefix is `.tmp`, so `/tmp/.tmp*` belongs to every Rust program on the machine. The reaper can only safely delete names this repo owns, which is why the root is not simply another `.tmp*` dir.

## Where the root lives

The temp base defaults to `/var/tmp/dad-e2e-<uid>` — **not** `/tmp`, and deliberately **not** anywhere under the checkout.

The reason it is not `/tmp` is that `/tmp` is a tmpfs on this project's dev box, so every leftover root is resident RAM rather than disk. Measured on `main` at `d3ea031`: 280 leaked roots totalling 6.2 GB accumulated in four hours, with swap down to **5 MiB free**; reaping them returned 3.8 GiB of swap, which is headroom `rustc` needs for the e2e compile. The failure that follows does not look like an out-of-space error — `dispatch_013` went **122s FAIL → PASS in 8.9s** with nothing changed but the temp location.

`/var/tmp` is the replacement because it is short, and because the FHS requires it to survive reboots, which in practice means distributions do not back it with a tmpfs. That is a **convention, not a runtime guarantee**: nothing here calls `statfs` to check the filesystem type, so a machine that has deliberately mounted `/var/tmp` on tmpfs gets the old behaviour back with no warning. The same caveat applies to any `DAD_E2E_TMPDIR` you set. `df -h /var/tmp` and `findmnt -T /var/tmp` are the two commands that actually answer the question.

### Why not `target/`

Putting the base under the repo's own `target/` was the first attempt at this fix, and it is worse than it looks. Every seeded fixture would then be a **descendant of the real checkout**, which carries `CLAUDE.md`, `AGENTS.md`, `.claude/` and `.agents/`. Real agents walk ancestors and would pick up genuine project instructions and skills, and the Codex worker runs `workspace-write` from that directory — so a test's effective writable workspace could be the live repository. A nested `git init` does not close it: a git root is not a filesystem boundary, and several real-agent tests (`e2e_delegate_work_done_chain`, `e2e_pi_worker`, `e2e_codex_worker`, `e2e_pi_orchestrator`) call `race_safe_tempdir()` with no `git init` anywhere near them. If you specifically want a target-local base, set `DAD_E2E_TMPDIR` to one — that is an explicit choice, and it is not made for you.

### Why a private parent rather than `/var/tmp` itself

`/var/tmp` is mode **1777** — world-writable, sticky, shared by every user on the machine. Roots placed directly in it are indistinguishable by name from a `dad-tests-*` directory belonging to somebody else, which makes both halves of the problem unpleasant: a reaper that trusts the name can erase another user's credential-bearing sandbox, and one that does not usually just fails. Nesting everything inside a parent that is verified to be **0700 and owned by the effective UID** removes the question by construction — nothing under it can belong to another user.

The parent is created with the mode applied by `mkdir(2)` itself, never chmod'ed afterwards. If it already exists it is **verified, not repaired**: not a symlink, owned by us, exactly mode 0700. A parent that fails verification is left exactly as it is and the harness **stops** — it does not fall through to the next candidate, because that candidate is usually the RAM-backed system temp dir and quietly landing there turns a security refusal back into the capacity problem this whole page is about. The failure names the path, what was observed (owner and mode) against what is required, and the remedy (`ls -ld` then `rm -rf`, or point `DAD_E2E_TMPDIR` somewhere else). A parent that is merely **absent** — no `/var/tmp` at all, a non-Unix platform, a read-only filesystem — is an ordinary environment difference and still falls through with a warning; only a directory that is *present and untrustworthy* is fatal.

### The ladder

The one thing that can veto a candidate is **path length**. These directories hold Unix domain sockets, and `sockaddr_un::sun_path` caps at 108 bytes on Linux and 104 on macOS/BSD. `/tmp` costs 4 characters where a `<worktree>/target/tmp` costs 60+, and this repo's worktree scheme (`../<repo>-<suffix>`, used by `/worktree-prd` and `/verify-pr`) reaches that routinely — in `dot-agent-deck-dispatch-tmpfs-322` an `attach.sock` at the harness's usual depth is 115 bytes and `bind(2)` fails with `AF_UNIX path too long`. `/var/tmp/dad-e2e-1000` is 21 bytes against a 55-byte allowance, so it has 34 to spare.

| # | Candidate | Notes |
|---|---|---|
| 1 | `$DAD_E2E_TMPDIR` | Explicit. Validated (see below), then honoured — including when it is too deep for a socket, which warns rather than silently relocating. What is honoured is the *resolved* form, so a symlinked value is followed exactly once, here. A value that fails validation is fatal, never demoted to candidate 2. |
| 2 | `/var/tmp/dad-e2e-<uid>` | The default. Unix only; created 0700 or verified as ours. Absent means fall through; present but untrustworthy is fatal. |
| 3 | `std::env::temp_dir()` (`TMPDIR`, else `/tmp`) | Last resort, and the only rung on Windows. This is the one outcome that can put the suite back on a RAM-backed filesystem, so it always prints a warning. |

`TMPDIR` on its own no longer relocates the harness root — it only reaches candidate 3. Use `DAD_E2E_TMPDIR` to move the harness deliberately. `CARGO_TARGET_DIR` has no effect on the temp base at all any more.

### What `DAD_E2E_TMPDIR` is checked for

It is not taken verbatim. It must be **absolute** and free of `..` — a relative value would resolve against whatever working directory a test binary happens to have, and `..` silently widens the scope of everything downstream. `Path::is_absolute` is what decides, so this is a per-platform question: `/var/tmp/e2e` is absolute on Unix and is *not* on Windows, where a path without a drive letter resolves against whatever drive is current.

What happens next is a single resolution followed by a descriptor walk:

1. **Validated before it is resolved.** The value is walked from `/` one component at a time, and a symlink is inspected **without being followed**: only one owned by root or by the effective UID is resolved, and only then are its own components pushed onto the walk and traversed like any others. Symlinked ancestors are resolved rather than refused because they have to be — on macOS `/var` is a symlink to `/private/var`, so `/var/tmp` and the platform's own `std::env::temp_dir()` have a symlinked ancestor on a completely healthy machine, and refusing them rejected the entire platform. What comes back is the resolved spelling, so a link is followed exactly once, here, and never again downstream. **The ordering is load-bearing.** An earlier cut of this handed the value to `canonicalize` first and walked only the result, which discarded every symlink entry before its owner was ever looked at. On a multi-user host that is exploitable: the operator asks for `/var/tmp/my-dad/base`, another user creates `/var/tmp/my-dad` as a symlink to the operator's own checkout first, and `/var/tmp` being sticky then works *against* the victim — sticky stops them removing or renaming the planted entry. The resolved checkout was walked as a chain of perfectly ordinary victim-owned ancestors and accepted, with `base` created 0700 inside the live repository and real agents running below it. Accepting a sticky 1777 ancestor is only sound when the entry found *below* it is judged, which is what this now does. A `..` inside a link *target* is refused rather than resolved; no system link the harness needs contains one, and following one would step back above a component already proved safe. Link chains are bounded at 40 hops, the kernel's own `ELOOP` cap.
2. **Walked with descriptors, not names.** The resolved path is descended one component at a time with `openat(2)` and `O_NOFOLLOW | O_DIRECTORY`, and each directory is judged by `fstat(2)` on the descriptor it was opened with. A `stat` of a path followed by a *use* of that same path is two lookups, and anything with write access to a component can change what the second one finds; here, the permission check and the next step's starting point are the same object. Ancestors must be owned by **us or by root** and must not be **group/world-writable without the sticky bit**. Sticky 1777 directories such as `/tmp` and `/var/tmp` are fine as ancestors — the sticky bit is exactly the guarantee that only an entry's owner may rename or remove it.
3. **Created with `mkdirat`, which refuses to adopt.** Missing components are created owner-only by `mkdir(2)` itself (never chmod'ed afterwards), one at a time, and `mkdirat` *fails* with `EEXIST` rather than accepting whatever occupies the name. A component that was missing when the path was resolved can be created by another local user before the harness reaches it, and a recursive create would take their directory — or their symlink — sight unseen. `EEXIST` is therefore not success: it falls through to the same open-and-judge a freshly created directory gets.
4. **The base itself is revalidated at the end**, on the descriptor the walk finished on, under the strict rule: a real directory, owned by the effective UID, at **exactly mode 0700**. That is the same bar the default `/var/tmp/dad-e2e-<uid>` parent has to meet, and for the same reason — this is where the harness seeds real agent credentials. It applies whether the harness created the directory or found it, so pointing the variable at an existing 0755 directory fails with `mode is 0o755, not the 0o700 the harness requires`; `chmod 700` is the fix, and the directory is refused rather than repaired. The check is *exact*, not merely "no group or other bits": that weaker mask also accepts 0500, 0300, 0000 and 1700. None of those is a confidentiality problem — `mkdir(2)` applies the mode and a umask can only clear bits — but a pre-existing 0500 directory used to pass the pre-flight whose whole job is to name the problem up front and then fail much later as a bare `Permission denied`. The refusal names the innocent cause too: a umask that clears **owner** bits (`umask 0200` yields 0500) produces exactly this from a directory the harness created itself.

What this does **not** claim: the harness ends up holding a validated *path*, not an open handle, so every later use resolves the name again. What makes that safe is the property proved on the way down — no ancestor is writable by another unprivileged user except under the sticky bit, where only an entry's own owner may rename or remove it — not the walk itself. Closing the remaining gap would mean keeping the descriptor and doing every subsequent operation relative to it, which is a much larger change than a temp-directory chooser warrants.

### What happens on Windows instead

There are no POSIX ownership or mode bits to judge a candidate by there, and no `openat`-shaped API reachable from `std`, so steps 2–4 above have no equivalent. What the non-Unix arm does is the same walk **by name**: the shape check, then one component at a time, each stat'ed with `symlink_metadata` and refused if it is a symlink, a junction, any other reparse point, or not a directory. A component that is missing is created with `create_dir`, **not** `create_dir_all` — so it fails with `AlreadyExists` rather than accepting whatever appeared in the meantime, and `AlreadyExists` is judged rather than treated as success. `FILE_ATTRIBUTE_REPARSE_POINT` is checked directly rather than relying on `FileType::is_symlink`, which is true only for the two tags `std` classifies as links (symlinks and junctions) and misses cloud-file placeholders and `AppExecLink` entries; the attribute bit is reachable from `std` via `MetadataExt`, so this costs no dependency.

That closes the *silent adoption*: a pre-planted entry can no longer redirect the credential-bearing harness tree into storage somebody else chose. Three things it does **not** close, and cannot from `std`:

1. The judgement is a **second lookup of the name**, not an `fstat` on the descriptor the entry was opened with. The name can be swapped between the `AlreadyExists` and the `symlink_metadata`, and again before every later use. The window is narrowed, not removed.
2. There is **no ownership check**, so a plain directory another local user planted at a missing component is still adopted — Windows ACLs are not reachable from `std` and there is no `uid` to compare. Redirection is refused; a co-located directory owned by somebody else is not detected.
3. Directories are created with **inherited ACLs**, not the 0700 equivalent `mkdir(2)` gives the Unix arm, so a permissive parent stays permissive.

All three are the ACL-and-handle work tracked by #163/#164, deliberately not fixed with a `windows-sys` dependency for a platform whose L2 tier does not run yet. One deliberate difference from the Unix arm: a symlinked **ancestor** is refused rather than resolved. Resolving is a concession the Unix side has to make because macOS's own `/var` is a symlink to `/private/var`; Windows has no such component on a healthy machine, and `canonicalize` there returns a `\\?\` verbatim path that would then be the spelling every message and length budget used.

A rejected value **is** fatal, and more plainly so than a rejected default: setting the variable states where the temp dirs must go, so a value that cannot be honoured — for any reason, including "could not be created" — stops the harness rather than being ignored. There is no reading of an explicit instruction under which silently doing something else is the helpful answer.

## Why an `atexit` hook rather than `Drop`

The lock dir this replaced was held in a `static OnceLock<TempDir>`. **Rust does not run destructors for statics at process exit**, so its `TempDir::drop` never fired. Because nextest runs one process per *test*, that leaked one directory per test — on a fully green run. Measured before the fix: 13 directories from 56 passing tests, and 6,667 accumulated on one dev machine over eight days.

If you ever need process-lifetime scratch state, do not reach for a `static TempDir`. Put it under the harness root and let the exit hook take it.

## Reading a leftover: 0775 means the sweep *worked*

A leftover `dad-tests-*` directory at mode **0775 is not a harness root**. The harness creates its root with `mkdir(2)` at 0700 and panics if the mode it reads back is anything looser, so a root it created can never be observed at 0775. What you are looking at is a *re-creation*: the exit hook removed the real root, and an agent process the test spawned outlived it and wrote into the `$HOME` it still had — `mkdir -p` walked the deleted chain back into existence at the umask default (0775 under the common `umask 002`).

The forensic signature, all four of which held for every one of the 32 leftovers observed after one full e2e run:

- **mode 0775** on the root *and* on the `.tmpXXXXXX` per-test dir inside it — both are created 0700 and both are asserted, so neither can be a survivor;
- **no fixture content** — the fixture copy is the first thing that happens after the per-test dir is created, so a genuine root always has it; a skeleton has only the subtree the orphan re-made (typically just `home/`);
- the root's **birth time equals its mtime**, meaning nothing was ever created in it after the single child that re-made it;
- the root's **birth time equals the inner dir's birth time**, because one `mkdir -p` made both.

A root left by an abnormal termination looks like the opposite of all four: `SIGKILL` skips the exit hook, so the whole tree survives at 0700 with the fixture, `.git` and the seeded `HOME` intact.

Re-measured on `5e8e0ed` after a full recorded `cargo test-e2e`, on the 15 surviving roots: **14 at 0775 and 1 at 0700**, and the split lands exactly where the signature predicts. All 14 of the 0775 roots carry it — 0775 on the root *and* on the inner `.tmp*` dir, `birth == mtime`, `root.birth == inner.birth`, no `.git` anywhere, and only the subtree an orphan re-made. One of them is explicit about the mechanism: `dad-tests-1834462-uzviq5` holds an **opencode** agent's own writeback (its snapshot object store, and the work-done report it wrote), first byte landing **9 seconds after the root's birth time** and the last at 22 seconds, with every leaf at 0664 — the umask default, where the harness writes leaves 0600. The single 0700 root is the genuine SIGKILLed survivor: 285 MB, seeded `HOME` intact, `.claude` versions and MCP logs and all. So the 0775 population is orphan writeback after a **successful** sweep, not a breach of the root's exact-mode invariant, which is asserted at creation inside `harness_temp_root()` and cannot describe a directory the harness did not create. Note the `birth == mtime` mark on its own is weak — a genuine root gets its single child immediately too, so it holds either way. The discriminating marks are the **mode** and the **absent fixture**.

The distinction matters when you are judging whether cleanup regressed. Skeleton residue is evidence the sweep **ran** — the fixture is gone precisely because it succeeded — and it cannot be fixed by making the sweep more reliable, only by reaping the spawned processes before the sweep or re-sweeping after them. That is the leak-*rate* question tracked separately in #461. Either way the residue stays reclaimable: the reaper keys on the directory *name*, never on its mode.

There is no live exposure while this sits on disk. The private parent is 0700, so no other user can traverse into it whatever the modes inside say.

## Reaping leftovers

```bash
cargo xtask clean-e2e-tmp                      # dry run — always start here
cargo xtask clean-e2e-tmp --apply              # actually delete
cargo xtask clean-e2e-tmp --older-than 1 --apply
cargo xtask clean-e2e-tmp --root /my/base --apply   # a base you moved yourself
```

Dry-run is the default and `--apply` is required to delete anything. By default it only considers directories this repo owns:

| Prefix | Reaped by default | Why |
|---|---|---|
| `dad-tests-*` | yes | The current harness root. |
| `dad-unit-*` | yes | `src/test_temp.rs` scratch dirs, from tests that do not link the harness. Not roots: no fixture, no seeded HOME, and no exit hook behind them, so this command is the only thing that reclaims one after a SIGKILL. |
| `dot-agent-deck-test-lock-*` | yes | Pre-fix lock dirs, still present in bulk on older machines. |
| `.tmp*` | **no** — needs `--include-untagged` | The `tempfile` crate's default prefix, shared with every Rust program on the machine. |

Only pass `--include-untagged` when no other Rust build or tool is running; it can otherwise delete a live temp dir belonging to something else. Because of that it is **restricted to the system temp dir** (the historical location the advice was written for) and to any *other* directory you name with `--root`. It never applies to the private `/var/tmp` parent — including when you name that parent with `--root` yourself, because a root's treatment is decided by the directory it resolves to and not by how it was spelled. Directories younger than the age threshold (default 6h) are always left alone so a reap cannot race a running suite, and symlinks are never followed.

### Which directories it looks in

The **standard** roots: the private `/var/tmp/dad-e2e-<uid>` parent and the system temp dir. Roots that are absent are skipped silently; two spellings of one directory (a symlink, a `TMPDIR` with a trailing `/.`) are de-duplicated by the directory they resolve to, not by how they are written.

Every root is **vetted before a single entry is read**, and the private parent is vetted strictly. It has to be: `/var/tmp/dad-e2e-<uid>` is a predictable name in a world-writable directory, so another local user can occupy it before this user's first run — and it is the *harness* that verifies that parent before writing under it, while this command is the half that **deletes**. "Ours by construction" is therefore not something the reaper may inherit. It requires a real directory, owned by the effective UID, with no group or other bits, inside a `/var/tmp` that is itself a root-owned (or ours) sticky directory; a symlink at that name is **refused, never followed**, and so are a FIFO, a plain file and a dangling link. A refused standard root is named on stderr and skipped — nothing under it is read or removed — and a refused `--root` is a hard error. Scanning and deletion then both run against the path vetting resolved, not the spelling it was handed, so a symlinked component cannot be retargeted between listing a directory and removing it.

What that still does not do is hold each root open as a descriptor and enumerate relative to it: `std` offers neither `read_dir` nor `remove_dir_all` from a file descriptor, so it would mean an FFI directory walk of its own. The residual is one lookup wide — between the listing and the removal, an entry could be swapped by whoever can write in the root. Under the private parent (0700, proved ours) nobody can; under a sticky system temp dir only the entry's own owner can, and ours are ours; under a hand-named `--root` it is your directory and your call.

That is *this* machine's, *this* checkout's picture. It cannot infer another worktree's leftovers, or a `DAD_E2E_TMPDIR` that is no longer exported — **run it in the worktree the leaking run ran in**, or name the directory with `--root`.

`--root` is also how you reap a base you moved with `DAD_E2E_TMPDIR`, which is deliberately *not* scanned automatically: where the harness may write and what a delete command may remove are different trust decisions, and one should not silently grant the other. When the variable is set but not passed, the reaper prints a note naming it and showing the `--root` invocation. Passing `--root` **replaces** the standard set rather than adding to it, so a deliberate scan of one directory cannot quietly also delete from `/var/tmp` or `/tmp`. Naming the private parent itself is the one case that is *not* treated as a hand-named directory: it is recognised by resolved path and keeps the strict vetting above, so pasting the pre-flight's own `--root` line cannot buy that directory a weaker check than discovering it as a standard root would.

## Pre-flight space check

Under `--features e2e`, the harness checks free space on the temp base it actually chose — not a hardcoded `/tmp` — before allocating its root, and fails with a message that leads with `HARNESS PRE-FLIGHT FAILURE … NOT a product regression` and names the path, the requirement, the shortfall and the remedy (including the `--root` form, since a base you moved yourself is not one the reaper scans by default). This exists because an exhausted temp filesystem does *not* look like an out-of-space error — it surfaces as agents never becoming input-ready, `git init` failing, and daemons never booting, which reads like a product regression. One diagnosis of this cost a full round trip.

It is one `statvfs` per test process, at the single choke point every harness temp dir passes through. It is also deliberately incapable of becoming a new flake: a filesystem whose free space cannot be queried produces no verdict at all, and `DAD_E2E_MIN_FREE_MB=0` switches it off entirely.

The 2 GB default is a "this run is doomed" floor rather than a capacity guarantee. Peak demand is what matters — one seeded HOME measures 263–284 MB and nextest runs one process per core, so eight concurrent tests already want ~2.2 GB — but the threshold is set below true peak so it catches the exhausted-tmpfs case without tripping on a modest CI runner.

## Environment variables

| Variable | Default | Effect |
|---|---|---|
| `DAD_E2E_TMPDIR` | unset | Temp base for the harness root. Validated (absolute, no `..`, then walked from `/` with `openat`/`O_NOFOLLOW`, validating each component *before* resolving it; ancestors not replaceable by another unprivileged user, symlinks followed only when owned by you or root, the base itself owned by you at exactly 0700 — on Windows, the weaker by-name walk described above), then outranks every other candidate — including the socket-length veto, which only warns. A value that fails validation stops the harness rather than being ignored. |
| `TMPDIR` | system temp | Only reaches the last-resort candidate (3). It no longer relocates the harness root on its own — use `DAD_E2E_TMPDIR`. |
| `DAD_E2E_MIN_FREE_MB` | `2048` | Free space the e2e tier requires on the chosen base. `0` disables the check. |
| `DAD_E2E_IMPORT_CLAUDE_PLUGINS` | unset (off) | Set to `1` to copy the host's `~/.claude/plugins` into every seeded HOME. Off by default: it is ~11 MB per HOME, nothing in the suite depends on it, and with dozens of tests running concurrently it is a real share of peak temp demand. |

`CARGO_TARGET_DIR` no longer affects any of this.

## Leftovers hold real credentials

The harness copies the host's agent auth state into every seeded HOME so real-agent tests can run (issue #358 tracks narrowing that). Cross-user read access is blocked — leaves are written 0600, the root is 0700, and the `/var/tmp` parent is 0700 — but the *lifetime* changed with the move: `/tmp` is usually cleared at boot, `/var/tmp` is required not to be. A SIGKILLed run therefore leaves real Claude/OpenCode/Codex auth state on durable storage until something removes it.

The expectation is that leftovers do not outlive a working day: run `cargo xtask clean-e2e-tmp --apply` at the end of a session where the suite was interrupted, and treat anything older than the 6h default threshold as something to reap rather than something to leave. It is a retention expectation, not an enforced one — nothing expires those directories on its own.

## On macOS

Three differences worth knowing. None of them changes the design, but the third is a real coverage gap rather than a platform quirk.

**`/var` is a symlink to `private/var`**, so the harness's `/var/tmp/dad-e2e-<uid>` parent really lives at `/private/var/tmp/dad-e2e-<uid>`. That is accepted rather than refused, at both ends, and the reason is that `symlink_metadata` does not follow only the *final* component: `symlink_metadata("/var/tmp")` traverses the `/var` link and stats the real `/private/var/tmp`, which is `drwxrwxrwt root:wheel` — a root-owned sticky directory, exactly what the shared-holder rule asks for. The link itself is only ever *judged* where it is a component of a `DAD_E2E_TMPDIR` walk, and it is root-owned there too, which is why that walk resolves links owned by root instead of refusing them (see step 1 of [what the variable is checked for](#what-dad_e2e_tmpdir-is-checked-for)). The descriptor walk itself uses `O_RDONLY` there rather than Linux's `O_PATH`, which is the portable spelling and costs only the difference between needing *read* and needing *search* permission on an ancestor — no component on the way to `/var/tmp` or to a `$HOME` is search-only.

The consequence is that the two halves hold **different spellings of one directory**. The harness joins the parent's name onto `/var/tmp` and never canonicalises, so `/var/tmp/dad-e2e-501` is what the socket budget is charged against and what `bind(2)` actually sees; `cargo xtask clean-e2e-tmp` resolves each root exactly once and scans `/private/var/tmp/dad-e2e-501`. Nothing keys on the string: de-duplication, the `DAD_E2E_TMPDIR` hint and root identity all compare canonicalised paths, and the owned-prefix match reads the **final** component, which resolution never rewrites. Both spellings fit comfortably — 20 and 28 bytes against a 55-byte base allowance, composing to 68 and 76 against the 103-byte socket path. `SUN_PATH_USABLE` is already macOS's smaller figure, so nothing needs recalibrating for the platform.

**The default temp dir is per-user and disk-backed.** `std::env::temp_dir()` there is `$TMPDIR`, which launchd sets to a per-user `/var/folders/…/T/` on the boot volume — macOS mounts no tmpfs by default. The RAM-backed problem this whole page is about is therefore a **Linux** problem: on a Mac the last-resort rung costs disk rather than memory, and what the private `/var/tmp` parent buys you there is the shorter socket path and one place to reap, not memory pressure. `--include-untagged` is correspondingly less dangerous, since that per-user directory is not shared the way Linux's `/tmp` is.

**The reaper's own tests do not run on macOS CI.** `build-macos` runs `cargo nextest run` with no `--workspace`, so cargo's default target selection is the root package alone and the `xtask` crates' tests never execute there at all — [issue #470](https://github.com/vfarcic/dot-agent-deck/issues/470). Everything in `tests/common/` *does* run on macOS (13 fast-tier integration binaries include it), so the harness half of this page is covered on the real platform; the `clean-e2e-tmp` half is not. The macOS-shaped cases for it are therefore pinned on Linux instead, by building a `var -> private/var` symlink over a sticky `private/var/tmp` in a scratch directory and driving `vet_root` at it. The one thing that shape cannot reproduce unprivileged is the `root:wheel` ownership, so that is driven through the pure verdict functions with injected values, as everything else foreign-owned in this file is.

## A note on tmpfs

If `/tmp` on your machine is a tmpfs, every leftover is resident memory rather than disk, and the failure mode is self-amplifying — a run that dies mid-test leaves more behind, so the next run has less headroom. That is why the default base moved off it. Measured on an NVMe machine, moving the suite off tmpfs cost no measurable wall-clock time (fast tier 24.5s either way).

The tradeoff this default accepts is that leaks stop causing red runs and instead accumulate quietly. That is what the reaper and the pre-flight check above are for — run `cargo xtask clean-e2e-tmp` occasionally rather than waiting for a suite to go red.
