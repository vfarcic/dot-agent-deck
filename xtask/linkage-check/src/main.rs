//! PRD #77 catalog ↔ test linkage check + `xtask` subcommand
//! multiplexer.
//!
//! Invoked as `cargo xtask <subcommand>` (alias in `.cargo/config.toml`).
//! Subcommands:
//!
//! - `linkage-check` (default) — first runs a repository-state preflight
//!   (issue #557; see [`repo_state`]), then performs the eleven checks
//!   listed in Decision 7 + Decision 30:
//!
//!   The preflight is deliberately not one of the numbered checks: it answers
//!   "is this repository sane to reason about", a different question from
//!   "does the catalog match the tests", and it runs first so a repository
//!   in a state that would misdiagnose the checks below is caught before
//!   any of them run. It asserts that the object store is not unexpectedly
//!   shallow and that the worktree registry has not drifted from what is on
//!   disk — both gated so a legitimately shallow, single-worktree CI clone
//!   is exempt by construction. See [`repo_state`] for the full reasoning.
//!
//!   1. Every catalog ID has at least one `#[spec("...")]` referencing
//!      it OR is on the allowlist (`m2.allowlist`).
//!   2. Every `#[spec("...")]` references a real catalog ID.
//!   3. Catalog IDs match the format regex.
//!   4. Function name carries the `<sub>_<NNN>` prefix (Decision 17).
//!   5. No raw `std::thread::sleep` / `tokio::time::sleep` /
//!      `for _ in 0..N` polling in `tests/e2e_*.rs` bodies (Decision 21).
//!   6. No `#[ignore]` on `#[spec(...)]`-annotated tests (Decision 26).
//!   7. Every `#[spec(...)]` test carries a `/// Scenario:` doc
//!      comment with a body AND `cargo xtask docs --tests` exits 0
//!      against the current source + catalog (Decision 30 / M4.3).
//!      The byte-identity diff against the on-disk `.md` is gone:
//!      `.dot-agent-deck/` is gitignored dev-time state and would
//!      not exist on a fresh clone.
//!   8. No bare `tempfile` constructor — directory (`tempdir()`,
//!      `TempDir::new()`) *or* file (`NamedTempFile::new()`,
//!      `tempfile()`) — anywhere under `tests/`, or in the files on
//!      [`EXTRA_TEMP_COVERED`]. Issue #322. See
//!      [`BARE_TEMPDIR_RULE`].
//!   9. No `crate::` path in `src/test_temp.rs`, which is
//!      `#[path]`-included by the lib target AND by every
//!      integration-test crate that needs a disk-backed scratch dir.
//!      Issue #474. See [`SELF_CONTAINED_RULE`].
//!  10. Every file under `tests/` that builds an `AgentPtyRegistry`
//!      or calls `run_daemon_with` also arms the wrapped-child
//!      lifetime bound — `common::init_test_env()` or
//!      `child_lifetime_bound::arm()`. Issue #668. See
//!      [`UNARMED_SPAWN_RULE`].
//!  11. No file under `tests/` pins a wrapped-child lifetime cap
//!      longer than the one `clean-e2e-tmp` derives its dead-owner
//!      deletion floor from. Issue #679. See
//!      [`overlong_lifetime_cap_rule`].
//!
//!   The numbers are stable identifiers in the failure output, so a
//!   new rule takes the next one rather than renumbering the others.
//!
//!   Checks 1/2/4/6 bind each `#[spec("…")]` to its test function
//!   through the SAME syn walker rule 7 uses
//!   ([`xtask_docs::discover_tests`]) rather than a line regex. Issue
//!   #406: the old regex matched `^\s*fn\s+` only, so an `async fn`
//!   test was invisible and its annotation silently re-bound to the
//!   next plain `fn` in the file — which either blamed an unrelated,
//!   correctly-named function for a prefix mismatch or, when that
//!   function happened to share the prefix, let a wrongly-named test
//!   pass unchecked. A text scan still locates every annotation so
//!   that one syn could NOT bind to a function is reported explicitly
//!   instead of drifting onto its neighbour.
//!
//! - `docs` — invokes the `xtask-docs` binary's logic (paired-`.md`
//!   generator). Forwards remaining args.
//! - `clean-e2e-tmp` — issue #322: reaps stale e2e harness temp dirs left
//!   behind by SIGKILLed test processes. Decides by whether the owning PID
//!   in the `dad-tests-<pid>-*` name is still alive rather than by age
//!   (issue #461). Dry-run unless `--apply`.
//! - `list-tests` — PRD #77 Decision 31: emits a Markdown report of
//!   every `#[spec]` test created or modified in this branch versus
//!   `origin/main`, plus per-catalog-entry prose diffs and any
//!   `m2.allowlist` changes. The orchestrator surfaces this to the
//!   user before delegating release.
//!
//! Exits 0 on success, 1 on any failure with a per-finding summary.

mod clean_tmp;
/// Issue #603: the adaptive issue labeler's post-agent memory validator. Tests
/// only — the rule lives in the agentic workflow, and these drive the real
/// script under `node`.
#[cfg(test)]
mod issue_labeler_memory;
mod list_tests;
/// Issue #648: the toolchain pins duplicated between `devbox.json` and
/// `.github/workflows/`. Tests only, and Unix only — the rule itself lives in
/// `scripts/check-pin-lockstep.sh`, which CI's `devbox` job also runs directly.
#[cfg(all(test, unix))]
mod pin_lockstep;
/// PRD #740: the job-graph properties in `release.yml` that keep a desktop
/// bundler failure off the CLI release. Tests only — nothing can run that
/// workflow outside a tag, so a bad edit is otherwise observable only after a
/// release has gone out wrong.
#[cfg(test)]
mod release_workflow_wiring;
mod repo_state;
/// PRD #740: `desktop/scripts/prepare-sidecar.sh`'s Windows filename rule.
/// Tests only, and Unix only — the rule lives in the script, which no CI job
/// runs today because nothing cuts a Tauri bundle yet.
#[cfg(all(test, unix))]
mod sidecar_staging;
/// Issue #521: the `/verify-pr` scripts' `KEY=value` output contract. Tests
/// only — there is no runtime rule here, the scripts enforce themselves.
#[cfg(test)]
mod verify_pr_stream;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regex::Regex;

// The Test-Case Catalog's permanent home. Relocated out of
// `prds/77-tui-testing-harness.md` (PRD #77 was archived to `prds/done/`,
// which broke the old hardcoded path) into a PRD-lifecycle-independent file.
const CATALOG_PATH: &str = "tests/CATALOG.md";
const ALLOWLIST_PATH: &str = "xtask/linkage-check/m2.allowlist";
const TESTS_DIR: &str = "tests";

/// Check 8 (issue #322): why a bare `tempfile` constructor is forbidden under
/// `tests/`, spelled out here because the violation is invisible at the call
/// site.
///
/// The harness redirects `tempfile`'s process-global default temp dir at its own
/// per-process root — but it can only do that from inside
/// `harness_temp_root()`'s lazy initialisation, i.e. the first time something
/// asks the harness for a directory. nextest runs one process per test, so a
/// bare `tempfile::tempdir()` that runs *before* any harness call in that test
/// is the first allocation of the process and lands in the OS temp dir instead:
/// commonly the RAM-backed `/tmp` this whole issue is about, at `tempfile`'s
/// default mode rather than 0o700, outside the free-space pre-flight, and — the
/// part that bites — left behind on SIGKILL under `.tmp*`, a name the reaper
/// deliberately will not touch by default because it belongs to every Rust
/// program on the machine.
///
/// This was not theoretical: `e2e_issue_dispatch` cloned whole repositories
/// through exactly that ordering. Rather than depend on every call site
/// happening to be preceded by a harness call, the suite calls
/// `common::harness_tempdir()`, which initialises the root first and then
/// allocates inside it. This rule is what keeps that true — the ordering
/// argument is invisible in a diff, so it cannot be left to review.
///
/// **Files, not just directories.** The rule originally matched only the
/// directory constructors, which left a hole inside the territory it claimed to
/// cover: `tempfile::NamedTempFile::new()` allocates in the OS temp dir on
/// exactly the same terms, and the Codex-auth pre-flight in `tests/common/`
/// used one. Measured on `5e8e0ed` as four zero-byte `/tmp/.tmp*` files. The
/// byte count is irrelevant — the rule's job is to keep the containment claim
/// true, and a constructor it cannot see makes the claim false. The `…_in`
/// forms (`tempdir_in`, `tempfile_in`, `new_in`) name their parent explicitly
/// and are therefore fine; the no-argument forms are not.
///
/// **Scope: all of `tests/`**, plus [`EXTRA_TEMP_COVERED`] for the lib target.
///
/// It used to be an enumerated list — `tests/e2e_*.rs`, `tests/common/`, and
/// two named files — because covering the rest of the fast tier was priced at
/// pulling `tests/common/mod.rs` into six more binaries and duplicating its
/// ~530 executions to contain small L1 `TempDir`s. That price was real when it
/// was measured, and it is no longer what the choice costs: `src/test_temp.rs`
/// is deliberately self-contained, so a fast-tier crate `#[path]`-includes it
/// for **two** extra executions. Six crates, twelve executions, measured — so
/// the enumeration outlived the measurement that justified it. A whole-
/// directory rule is also the only version a *new* file under `tests/` inherits
/// automatically; an enumerated one silently does not cover it.
///
/// The escape hatch is [`BARE_TEMPDIR_ALLOW`] on the same line, which the
/// harness's own defence-in-depth regression test uses — the one test whose
/// subject *is* the bare constructor.
const BARE_TEMPDIR_RULE: &str = "bare tempfile constructor — use `common::harness_tempdir()` / \
     `harness_tempfile()` (or `test_temp::tempdir()` outside the harness) so it \
     lands under the harness temp root even when it is the process's FIRST \
     allocation (issue #322)";

/// Files outside `tests/` that check 8 also covers.
///
/// `src/dispatch.rs` — lib-target unit tests that build real git repos and
/// worktrees. They do not link `tests/common/` at all and use
/// `crate::test_temp::tempdir()`; one of them was measured holding a live
/// 184 KiB `/tmp/.tmpYN3lNF` with a cloned repo in it during a recorded
/// `cargo test-e2e`, so the rule is what stops a bare constructor coming back.
///
/// The **rest** of `src/`'s unit tests are deliberately not here — ~82 call
/// sites across 22 files, a large mechanical diff that would move fast-tier
/// churn onto `/var/tmp` for no measured benefit. That is the one remaining
/// documented gap in `docs/develop/e2e-temp-dirs.md`; everything under
/// `tests/` is covered by the directory rule above.
///
/// Paths are repo-relative and compared with the platform separator
/// normalised, so this works on Windows too.
const EXTRA_TEMP_COVERED: &[&str] = &["src/dispatch.rs"];

/// Opt-out marker for check 8, on the offending line.
const BARE_TEMPDIR_ALLOW: &str = "linkage-check:allow-bare-tempdir";

/// The `tempfile` constructors that allocate in the **default** temp dir.
///
/// Directories: `tempfile::tempdir()`, `TempDir::new()`, `TempDir::with_prefix()`,
/// `TempDir::with_suffix()`, and the builder's `.tempdir()`. Files:
/// `NamedTempFile::new()`, `NamedTempFile::with_prefix()`,
/// `NamedTempFile::with_suffix()`, `tempfile::tempfile()`,
/// `spooled_tempfile()`, and the builder's `.tempfile()`. Every `…_in(parent)` /
/// `…_new_in(parent)` form names its destination and is deliberately NOT matched
/// — that is what the wrappers themselves call, and `…_in` sits between the name
/// and the `(` so none of the patterns here can reach it.
///
/// Factored out of `main` so it can be unit-tested; the file half of it was
/// missing for a while and nothing caught that.
///
/// **The `with_prefix` / `with_suffix` / `spooled` family was missing too**, and
/// the same argument applies: they are ordinary safe-looking constructors that
/// allocate in `std::env::temp_dir()`, verified present in the pinned
/// `tempfile 3.27.0` (`src/dir/mod.rs:269`/`:294`, `src/file/mod.rs:630`/`:657`),
/// and each has an `…_in` counterpart, so the rule is satisfiable. There was no
/// live call site when this was added — the value is that the guard now matches
/// the claim it makes in the module header ("no bare `tempfile` constructor …
/// anywhere under `tests/`") instead of enumerating a subset of it. A rule that
/// covers most of its stated territory is the shape that let
/// `NamedTempFile::new()` sit inside its own scope undetected.
fn bare_temp_ctor_re() -> Regex {
    Regex::new(
        r"tempfile::tempdir\s*\(|TempDir::new\s*\(|TempDir::with_prefix\s*\(|TempDir::with_suffix\s*\(|\.tempdir\s*\(\s*\)|NamedTempFile::new\s*\(|NamedTempFile::with_prefix\s*\(|NamedTempFile::with_suffix\s*\(|tempfile::tempfile\s*\(|spooled_tempfile\s*\(|\.tempfile\s*\(\s*\)",
    )
    .expect("bare temp constructor regex compiles")
}

/// Whether check 8 applies to `file`.
///
/// Everything under `tests/`, plus the explicit [`EXTRA_TEMP_COVERED`] list for
/// the lib target. `is_e2e` is no longer consulted — an `e2e_` file is under
/// `tests/` by construction — but stays in the signature because the caller has
/// it and because dropping it would make the two scoping rules look unrelated.
fn temp_ctor_rule_covers(file: &Path, root: &Path, tests_dir: &Path, _is_e2e: bool) -> bool {
    if file.starts_with(tests_dir) {
        return true;
    }
    EXTRA_TEMP_COVERED
        .iter()
        .any(|rel| file == root.join(rel).as_path())
}

/// Check 9 (issue #474): the one file in this repository that may not name its
/// own crate, spelled out here because nothing at the offending line says so.
///
/// `src/test_temp.rs` is compiled twice over: as an ordinary `mod test_temp` in
/// the lib target, and again inside every integration-test crate that pulls it
/// in with `#[path = "../src/test_temp.rs"] mod test_temp;` — ten of them when
/// this rule was written. In those crates `crate::` is the *test* binary's own
/// root, where nothing this repository defines is in scope, so one added
/// `crate::` path breaks every consumer at once.
///
/// **The self-containment is load-bearing economics, not style.** Containing
/// those fast-tier crates with `mod common;` instead was priced at pulling the
/// PTY harness into six more binaries and duplicating roughly **530** fast-tier
/// executions. The `#[path]` route cost **12** — measured, `cargo nextest list`
/// went 2,315 → 2,327, two per crate. The whole difference between those two
/// numbers rests on this one file staying free of `crate::`, and until this rule
/// that property was enforced by a comment.
///
/// It usually fails loudly, and the two ways that is not enough are the case for
/// a mechanical check. It fails as N identical `E0433`s that explain nothing
/// about why the file is unusual, so the obvious "fix" is to unpick the
/// arrangement rather than to drop the reference. And whether it fails at all
/// depends on which names the *consumer* happens to have at its own root:
/// measured with a `crate::features::experimental_enabled()` probe added to the
/// module, nine of the ten crates failed with `cannot find features in crate`
/// and `tests/features.rs` compiled clean, because its own
/// `use dot_agent_deck::features::{self, Features};` puts a `features` at that
/// test crate's root. So a `crate::` path can land green on the author's
/// `cargo test-fast` filter and break the next consumer to include the file.
///
/// **`super::` is deliberately not matched.** Inside the module's own
/// `mod tests` it names the module itself, which is both correct and used; at
/// file scope it would name the *including* crate's root and be exactly as
/// non-portable as `crate::`. Telling those two apart needs a parser rather than
/// a line scan, and no file-scope `super::` exists here — so this stays the
/// narrow guard issue #474 asked for rather than a second syn walker.
///
/// Scanned over the **comment-stripped** view, so the file's own header note can
/// point at this rule by name. A `crate::` inside a string literal is not exempt
/// ([`strip_rust_comments`] deliberately preserves literals); nothing in a
/// 40-line temp-dir resolver has needed one, and the diagnostic says which line
/// it is.
const SELF_CONTAINED_RULE: &str = "`crate::` path in a `#[path]`-shared file — this module is \
     compiled into the lib target AND into every integration-test crate that \
     `#[path]`-includes it, where `crate::` is that TEST crate's own root and \
     nothing the library defines is in scope. Keep it self-contained (`std`, \
     `libc`, `tempfile`, `super::`); anything it needs from the library has to \
     arrive as an argument instead. Sharing it this way is what costs 12 extra \
     fast-tier executions rather than the ~530 `mod common;` would (issue #474)";

/// The file check 9 guards. Repo-relative, joined onto the workspace root, so
/// the platform separator is whatever `Path::join` produces.
const SELF_CONTAINED_PATH: &str = "src/test_temp.rs";

/// The `crate::` paths check 9 forbids.
///
/// The leading `\b` is what keeps `some_crate::x` out: `_` is a word character,
/// so no boundary falls in front of that `crate`. `$crate::` from a
/// `macro_rules!` body IS matched, deliberately — it expands to the *defining*
/// crate's root and is non-portable for exactly the same reason. The optional
/// whitespace covers `crate ::`, which the compiler accepts.
///
/// Factored out so it can be unit-tested without a checkout, the same way
/// [`bare_temp_ctor_re`] is.
fn crate_path_re() -> Regex {
    Regex::new(r"\bcrate\s*::").expect("crate path regex compiles")
}

/// Every `crate::` path in `text`, formatted as `<display>:<line>: <rule>`.
///
/// Takes the contents rather than a path so its tests can feed synthetic
/// sources. A rule whose only coverage is the live checkout tests nothing
/// whenever that checkout is clean — which is its normal state, and would be
/// its state on the day the rule silently stopped matching.
fn self_contained_violations(display: &str, text: &str) -> Vec<String> {
    let re = crate_path_re();
    // Line endings are preserved 1-for-1 by the stripper, so these indices are
    // the raw source's line numbers.
    strip_rust_comments(text)
        .lines()
        .enumerate()
        .filter(|(_, line)| re.is_match(line))
        .map(|(idx, _)| format!("{display}:{}: {SELF_CONTAINED_RULE}", idx + 1))
        .collect()
}

/// Run check 9 against a workspace root.
///
/// An unreadable file is itself a failure. The guard's entire job is to outlive
/// edits to the arrangement it protects, and a rename that left this constant
/// behind would otherwise turn the rule into a no-op that still prints `ok` —
/// the same shape of silence the rule exists to end.
fn check_self_contained(root: &Path) -> Vec<String> {
    let path = root.join(SELF_CONTAINED_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => self_contained_violations(&path.display().to_string(), &text),
        Err(e) => vec![format!(
            "{}: cannot read the file check 9 guards ({e}) — if it moved, point \
             `SELF_CONTAINED_PATH` at its new home; if the `#[path]` sharing is \
             gone, delete the rule (issue #474)",
            path.display()
        )],
    }
}

/// Check 10 (issue #668): a test file that spawns agents must arm the wrapped-
/// child lifetime bound, spelled out here because nothing at the offending line
/// says so.
///
/// `dot-agent-deck wrap` bounds the wrapper (`arm_wrap_self_defense`) and, since
/// #661, the wrapped child's whole process group (`arm_child_group_backstop`,
/// which forks a reaper that outlives an uncatchable `SIGKILL` of the wrapper).
/// Both are gated on `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` so a *production*
/// wrapper forks nothing. Arming that gate is therefore the caller's job, and
/// `agent_pty::spawn` does not `env_clear` — it scrubs named deck vars and
/// inherits the rest — so one value in the test process reaches every child of
/// every spawn shape.
///
/// **This rule exists because forgetting it is silent and has already
/// happened.** #661 armed rows 1 and 2 of the suite's spawn table (`TuiDeck` and
/// `DaemonProc` both pin the cap after their `env_clear`) and nobody noticed row
/// 3 — the in-process `AgentPtyRegistry` path, 15 test files — was unarmed. The
/// symptom is not a failing test: it is a wrapped stand-in that survives the run
/// at `ppid=1` holding a deleted working directory, 221 of them censused on one
/// dev box with the oldest alive 9.4 days. Nothing in a diff shows the absence.
///
/// **Scope: `tests/` only.** `src/daemon.rs` calls `run_daemon_with` in
/// production, where the gate must stay unarmed, and would otherwise trip this
/// on every run.
///
/// **What it matches.** Constructing an `AgentPtyRegistry` (`::new()` /
/// `::default()`) or calling `run_daemon_with(…)`. Those two are the only ways a
/// test reaches an unarmed registry: everything else goes through
/// `common::spawn_inprocess_daemon` or `TuiDeck`, which arm it, and a test that
/// only borrows `daemon.pty_registry` had to build that daemon with
/// `run_daemon_with` first.
///
/// **What satisfies it.** `common::init_test_env()` for the files that link the
/// harness, or `child_lifetime_bound::arm()` for the ones that deliberately do
/// not (`tests/common/child_lifetime_bound.rs` is `#[path]`-includable on its
/// own, because `tests/common/mod.rs` is ~420 KB of PTY harness and pulling it
/// into a fast-tier crate to reach one `set_var` is a real compile cost). Either
/// marker anywhere in the file clears it.
///
/// **The view both halves are matched over** is comment-stripped AND
/// literal-blanked ([`blank_string_literal_contents`]), so prose about either
/// side is not a violation and neither is *quoted* code. That matters in both
/// directions, and unlike the limits below neither is accepted: a file that
/// merely mentions `child_lifetime_bound::arm()` inside a string — the obvious
/// shape for a test asserting this rule's own remedy text — must not thereby
/// clear its real spawn sites, and a file asserting on a message containing
/// `run_daemon_with(` must not fail the build for spawning nothing.
///
/// **Its false-negative surface, stated rather than hidden.** This is a line
/// scan, not a parser, so it is a belt: the load-bearing protection is the fd
/// fix in `src/wrap.rs`, which makes a stranded child die of its own hangup with
/// no env var involved. Three things it cannot see. It checks *presence*, not
/// *ordering*, so an `arm()` call placed after the spawn passes. It is
/// file-granular, so one armed test clears a second unarmed one in the same
/// file. And it keys on the module being included under its own name, so
/// `#[path = "common/child_lifetime_bound.rs"] mod bound;` defeats the marker.
/// All three are review-visible in a way the original gap was not, which is the
/// bar this rule is aiming at.
const UNARMED_SPAWN_RULE: &str = "agent spawn path with no lifetime bound armed — this file \
     builds an `AgentPtyRegistry` or runs a daemon in-process, so the agents it spawns inherit \
     THIS process's environment, and `dot-agent-deck wrap` leaves both its self-defence and \
     #661's child-group reaper unarmed without \
     `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` in it. A wrapped stand-in that outlives its wrapper \
     is then never reaped (issue #668). Fix: call `common::init_test_env()` before the first \
     spawn if the file links the harness, otherwise add \
     `#[path = \"common/child_lifetime_bound.rs\"] mod child_lifetime_bound;` and call \
     `child_lifetime_bound::arm()`. A deliberate exception pins its own cap per-`Command` and \
     carries `linkage-check:allow-unarmed-agent-spawn` on this line";

/// Opt-out marker for check 10, on the offending line. Same shape as
/// [`BARE_TEMPDIR_ALLOW`]: an exception is declared where it is taken, so review
/// sees it, rather than in a list far from the code.
///
/// No file needs it today. `tests/wrap_io.rs` and `tests/agent_lifetime_bound.rs`
/// are the two that pin their own per-`Command` caps deliberately, and neither
/// trips the rule — `wrap_io.rs` drives the `wrap` binary directly and builds no
/// registry, and `agent_lifetime_bound.rs` calls `init_test_env()` anyway.
const UNARMED_SPAWN_ALLOW: &str = "linkage-check:allow-unarmed-agent-spawn";

/// The spawn sites check 10 requires arming for.
///
/// `AgentPtyRegistry::default()` is matched alongside `::new()` even though no
/// call site uses it: it is the ordinary second constructor, and a rule that
/// covers most of its stated territory is the shape that let
/// `NamedTempFile::new()` sit undetected under check 8. `run_daemon_with` needs
/// its `(` — an import (`use dot_agent_deck::daemon::{Daemon, run_daemon_with};`)
/// names it without calling it and is deliberately not a violation.
fn agent_spawn_site_re() -> Regex {
    Regex::new(r"AgentPtyRegistry\s*::\s*(new|default)\s*\(|\brun_daemon_with\s*\(")
        .expect("agent spawn site regex compiles")
}

/// The calls that arm the bound, either of which clears check 10 for a file.
fn lifetime_bound_armed_re() -> Regex {
    Regex::new(r"\binit_test_env\s*\(|\bchild_lifetime_bound\s*::\s*arm\s*\(")
        .expect("lifetime bound arming regex compiles")
}

/// Check 11 (issue #679): no test may pin a wrapped-child lifetime cap longer
/// than the one `clean-e2e-tmp` derives its dead-owner deletion floor from.
///
/// `cargo xtask clean-e2e-tmp` reaps a root whose owning *test* process is dead
/// once the root is [`clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS`] × 2 old, and it
/// picks that multiple because a `setsid`'d daemon the test spawned can keep
/// writing under the root for as long as its own
/// `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` cap. So the floor is only ever as
/// sound as "no cap in this repository exceeds that number", and until #679
/// nothing checked it: the floor was hard-coded at 600 s and documented as
/// "2× the 300 s default" while `orchestration_dispatch_002` already pinned
/// **900** (issue #665), leaving a 300-second window in which `--apply` could
/// delete a root a live, still-entitled daemon was writing under.
///
/// Why a build-time text scan and not a runtime clamp.
/// `tests/common/child_lifetime_bound.rs`'s `clamped()` already bounds what the
/// test process *inherits*, and deliberately cannot reach this: `TuiDeck`
/// `env_clear`s and then applies the builder's `extra_env` **last**, so a
/// test's own `with_env` overwrites the harness pin and is handed to the child
/// verbatim. Raising that clamp's ceiling to legalise one value would weaken
/// the ambient guarantee for everyone, and reordering `extra_env` would break
/// the many other keys that rely on winning. A scan bounds the *written* pins
/// instead, which is the population the floor actually needs bounded.
///
/// Two shapes are matched, both against the comment-stripped source so prose
/// naming the variable is not a violation:
///
/// 1. the variable name adjacent to a literal value — `.env(VAR, "900")`,
///    `.with_env(VAR, "900")`, `(VAR, "300")`, `(VAR.into(), "300".into())`,
///    including the multi-line `.env(\n VAR,\n "900",\n)` spelling;
/// 2. a `const …MAX_LIFETIME…_SECS` binding with a literal value, which is how
///    `tests/common/mod.rs`'s `WRAP_TEST_MAX_LIFETIME_SECS = "120"` reaches the
///    nine `wrap_io.rs` sites that pass it by name rather than by literal.
///
/// Shape 2 is a proxy and is honest about it: a cap computed at run time, or
/// held in a constant named something else, is out of reach of any text scan.
/// Same standing as rule 10 — the belt, not the braces. What makes it worth
/// having anyway is that both shapes cover every pin that exists today, so the
/// *next* one to be written is the case it is for.
///
/// String literals are deliberately NOT blanked first, which rule 10 does do to
/// its input: here the pin *is* a pair of string literals, so blanking them
/// would empty the rule. The residual is that a file under `tests/` quoting one
/// of these two shapes inside a literal — a fixture holding Rust source, say —
/// would be reported. No file does today, and the remedy if one ever does is to
/// reword the fixture rather than to weaken the scan.
///
/// There is deliberately **no** per-line opt-out, unlike checks 8 and 10. Those
/// two guard a local choice a reviewer can reasonably wave through at the site.
/// This one guards a pair of numbers that must move together: a pinned cap *is*
/// how long an orphan may write, so the only correct response to a longer one
/// is to raise [`clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS`] — which raises the
/// floor with it — not to exempt the line.
fn overlong_lifetime_cap_rule(secs: u64) -> String {
    let cap = clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS;
    format!(
        "wrapped-child lifetime cap pinned at {secs}s, above the {cap}s \
         `clean-e2e-tmp` derives its dead-owner deletion floor from. A daemon this test \
         spawns `setsid`s out of its process group and may keep writing under the test's \
         temp root for the whole cap, but `cargo xtask clean-e2e-tmp --apply` would reap \
         that root {over}s earlier — deleting a live process's working directory (issue \
         #679). Fix: raise `MAX_PINNED_ORPHAN_CAP_SECS` in \
         `xtask/linkage-check/src/clean_tmp.rs` (which raises `DEAD_PID_MIN_AGE` with it) \
         and update `docs/develop/e2e-temp-dirs.md`, or pin a shorter cap here. There is \
         deliberately no per-line opt-out",
        over = secs.saturating_sub(cap)
    )
}

/// Shape 1: the variable name next to a literal value.
///
/// `\s*` spans newlines, which is what covers the multi-line `.env(` spelling
/// `tests/wrap_io.rs` uses. The optional `.into()` covers the owned-`String`
/// push in `tests/common/mod.rs`, and the optional `&` the borrowed spelling.
fn lifetime_cap_pin_re() -> Regex {
    Regex::new(r#""DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS"\s*(?:\.into\(\))?\s*,\s*&?\s*"(\d+)""#)
        .expect("lifetime cap pin regex compiles")
}

/// Shape 2: a constant holding the cap, matched by name.
///
/// Deliberately anchored on `MAX_LIFETIME` *and* a `_SECS` suffix so it reads
/// seconds and not, say, a variable-name constant — `MAX_LIFETIME_VAR: &str =
/// "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS"` in `tests/agent_lifetime_bound.rs`
/// carries no digits after the `=` and is correctly ignored.
fn lifetime_cap_const_re() -> Regex {
    Regex::new(r#"const\s+[A-Z0-9_]*MAX_LIFETIME[A-Z0-9_]*_SECS\s*:[^=;]+=\s*"?(\d+)"?"#)
        .expect("lifetime cap const regex compiles")
}

/// Every over-long lifetime cap in `text`, formatted as
/// `<display>:<line>: <rule>` and ordered by line.
///
/// Takes the contents rather than a path for the same reason
/// [`unarmed_agent_spawn_violations`] does: a rule whose only coverage is the
/// live checkout tests nothing whenever that checkout is clean, which is its
/// normal state and would be its state on the day it silently stopped matching.
fn overlong_lifetime_cap_violations(display: &str, text: &str) -> Vec<String> {
    let stripped = strip_rust_comments(text);
    let mut hits: Vec<(usize, u64)> = Vec::new();
    for re in [lifetime_cap_pin_re(), lifetime_cap_const_re()] {
        for caps in re.captures_iter(&stripped) {
            // An absurdly long literal saturates rather than failing to parse,
            // so a cap of `99999999999999999999` is reported, not skipped.
            let secs: u64 = caps[1].parse().unwrap_or(u64::MAX);
            if secs <= clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS {
                continue;
            }
            // The stripper replaces comment bytes in place and preserves every
            // newline, so the Nth newline here is the Nth newline of the raw
            // source and this is the raw line number.
            let start = caps.get(0).expect("whole match").start();
            let line = stripped[..start].bytes().filter(|b| *b == b'\n').count() + 1;
            hits.push((line, secs));
        }
    }
    hits.sort_unstable();
    hits.dedup();
    hits.into_iter()
        .map(|(line, secs)| format!("{display}:{line}: {}", overlong_lifetime_cap_rule(secs)))
        .collect()
}

/// Every unarmed spawn site in `text`, formatted as `<display>:<line>: <rule>`.
///
/// Takes the contents rather than a path so its tests can feed synthetic
/// sources, for the same reason [`self_contained_violations`] does: a rule whose
/// only coverage is the live checkout tests nothing whenever that checkout is
/// clean, which is its normal state and would be its state on the day it
/// silently stopped matching.
///
/// The arming marker is looked for across the whole file rather than per line —
/// arming happens once per process, usually in a harness helper hundreds of
/// lines from the spawn it protects.
fn unarmed_agent_spawn_violations(display: &str, text: &str) -> Vec<String> {
    let stripped = blank_string_literal_contents(&strip_rust_comments(text));
    if lifetime_bound_armed_re().is_match(&stripped) {
        return Vec::new();
    }
    let site_re = agent_spawn_site_re();
    // Line endings are preserved 1-for-1 by the stripper, so these indices are
    // the raw source's line numbers — and the raw line is what carries the
    // opt-out marker, since the stripper removes the comment it lives in.
    let raw_lines: Vec<&str> = text.lines().collect();
    stripped
        .lines()
        .enumerate()
        .filter(|(idx, line)| {
            site_re.is_match(line)
                && !raw_lines
                    .get(*idx)
                    .is_some_and(|raw| raw.contains(UNARMED_SPAWN_ALLOW))
        })
        .map(|(idx, _)| format!("{display}:{}: {UNARMED_SPAWN_RULE}", idx + 1))
        .collect()
}

fn main() -> ExitCode {
    // PRD #77 M4: route subcommands through this binary so the
    // single `cargo xtask` alias can drive both linkage-check and
    // docs. `cargo xtask docs --tests` → docs generator;
    // anything else (including no first arg or `linkage-check`) →
    // the seven Decision-7 / Decision-30 checks below.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("docs")) {
        return run_docs(&args[1..]);
    }
    if matches!(args.first().map(String::as_str), Some("list-tests")) {
        return run_list_tests(&args[1..]);
    }
    if matches!(args.first().map(String::as_str), Some("clean-e2e-tmp")) {
        return clean_tmp::run(&args[1..]);
    }

    let root = repo_root();

    // Repository-state preflight (issue #557): a different question from
    // the catalog↔test checks below, and one worth answering before any of
    // them spend seconds parsing the catalog. Runs first and short-circuits
    // on its own rather than joining `failures` below, so it stays a
    // preflight rather than becoming a ninth catalog check.
    let repo_state_failures = repo_state::run(&root);
    if !repo_state_failures.is_empty() {
        eprintln!(
            "linkage-check: repository-state preflight: {} failure(s):",
            repo_state_failures.len()
        );
        for f in &repo_state_failures {
            eprintln!("  {f}");
        }
        return ExitCode::FAILURE;
    }

    let catalog_path = root.join(CATALOG_PATH);
    let allowlist_path = root.join(ALLOWLIST_PATH);
    let tests_dir = root.join(TESTS_DIR);

    let mut failures: Vec<String> = Vec::new();

    let catalog_ids = match parse_catalog_ids(&catalog_path) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("failed to parse catalog at {}: {e}", catalog_path.display());
            return ExitCode::from(2);
        }
    };
    let allowlist = match read_allowlist(&allowlist_path) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "failed to read allowlist at {}: {e}",
                allowlist_path.display()
            );
            return ExitCode::from(2);
        }
    };

    // Check 3: format regex on catalog IDs.
    let id_re = Regex::new(r"^[a-z][a-z0-9-]*/[a-z][a-z0-9-]*/\d{3}$")
        .expect("catalog ID format regex compiles");
    for id in &catalog_ids {
        if !id_re.is_match(id) {
            failures.push(format!(
                "[3] catalog ID {id:?} does not match `<area>/<sub>/<NNN>`"
            ));
        }
    }

    // Scan tests/ AND src/ for `#[spec(...)]` annotations. PRD #83
    // added per-tab-selection `#[spec]` unit tests in `src/tab.rs`; the
    // e2e-only checks below key off the `e2e_` filename prefix, so
    // library sources never trip the sleep/polling rules.
    //
    // This text scan no longer decides which FUNCTION an annotation
    // belongs to — syn does that below (issue #406). It only records
    // where each annotation is, so an annotation syn could not bind is
    // reported at its own line.
    let mut test_files = collect_test_rs_files(&tests_dir);
    test_files.extend(collect_test_rs_files(&root.join("src")));
    let mut occurrences: Vec<SpecOccurrence> = Vec::new();
    let mut e2e_violations: Vec<String> = Vec::new();
    let mut ignore_violations: Vec<String> = Vec::new();

    let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("spec attr regex compiles");
    // Decision 21: forbidden in test bodies.
    let sleep_re =
        Regex::new(r"(std::thread::sleep|tokio::time::sleep)\b").expect("sleep regex compiles");
    let polling_re =
        Regex::new(r"for\s+_\s+in\s+0\.\.\s*\d+\s*\{").expect("polling regex compiles");
    let bare_tempdir_re = bare_temp_ctor_re();
    let mut bare_tempdir_violations: Vec<String> = Vec::new();
    let mut unarmed_spawn_violations: Vec<String> = Vec::new();
    let mut overlong_cap_violations: Vec<String> = Vec::new();

    for file in &test_files {
        let text = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read {}: {e}", file.display());
                continue;
            }
        };

        // M2.1 auditor Nit 5: strip line + block comments before running
        // the no-sleep regex check so a comment that mentions
        // `std::thread::sleep` (e.g. explaining why the harness does
        // NOT call it) does not register as a violation. The spec-
        // attribute scan uses the stripped copy too, so a commented-out
        // `#[spec(...)]` is not counted as a live annotation.
        let stripped = strip_rust_comments(&text);
        let raw_lines: Vec<&str> = text.lines().collect();
        let stripped_lines: Vec<&str> = stripped.lines().collect();
        let file_name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let is_e2e = file_name.starts_with("e2e_") && file_name.ends_with(".rs");

        for (id, line_no) in scan_spec_occurrences(&stripped_lines, &spec_re) {
            occurrences.push(SpecOccurrence {
                id,
                file: file.clone(),
                line: line_no,
            });
        }

        // Check 8 (issue #322): all of `tests/`, plus `EXTRA_TEMP_COVERED` for
        // the lib target. The e2e tier is where the allocations are whole cloned
        // repositories, where nextest's `slow-timeout terminate-after` SIGKILLs
        // a process before it can clean up, and where real agent credentials
        // get seeded — but the fast tier is no longer excluded, because the
        // exclusion was priced at pulling the PTY harness into six more
        // binaries and the `#[path]`-included `src/test_temp.rs` costs two test
        // executions per crate instead. Its files bind Unix domain sockets and,
        // on SIGKILL, survive as untagged `.tmp*` the reaper will not remove by
        // default. Run against the stripped view so a comment naming the
        // constructor is not a violation, but report the raw line number.
        if temp_ctor_rule_covers(file, &root, &tests_dir, is_e2e) {
            for (idx, raw) in raw_lines.iter().enumerate() {
                let stripped_line = stripped_lines.get(idx).copied().unwrap_or("");
                if bare_tempdir_re.is_match(stripped_line) && !raw.contains(BARE_TEMPDIR_ALLOW) {
                    bare_tempdir_violations.push(format!(
                        "{}:{}: {BARE_TEMPDIR_RULE}",
                        file.display(),
                        idx + 1
                    ));
                }
            }
        }

        // Check 10 (issue #668): `tests/` only — `src/daemon.rs` calls
        // `run_daemon_with` in production, where the gate must stay unarmed.
        // The whole-file arming lookup is inside the helper, which does its own
        // comment-stripping so it can be unit-tested against synthetic sources
        // rather than only against a checkout that is clean by construction.
        if file.starts_with(&tests_dir) {
            unarmed_spawn_violations.extend(unarmed_agent_spawn_violations(
                &file.display().to_string(),
                &text,
            ));

            // Check 11 (issue #679): `tests/` only, for the same reason. The
            // variable gates a TEST-only backstop, so `src/` never pins it —
            // `src/agent_pty.rs` only names it — and the population the
            // reaper's floor has to bound is exactly the pins written here.
            overlong_cap_violations.extend(overlong_lifetime_cap_violations(
                &file.display().to_string(),
                &text,
            ));
        }

        if is_e2e {
            // Check 5: forbidden waits / polling in e2e test bodies.
            // Run against the stripped (comment-free) view so a
            // commented-out `// std::thread::sleep` doesn't trip the
            // check, but keep the raw line numbers in the error message
            // so violators are easy to locate.
            for (idx, _raw) in raw_lines.iter().enumerate() {
                let stripped_line = stripped_lines.get(idx).copied().unwrap_or("");
                if sleep_re.is_match(stripped_line) {
                    e2e_violations.push(format!(
                        "{}:{}: forbidden sleep call (Decision 21)",
                        file.display(),
                        idx + 1
                    ));
                }
                if polling_re.is_match(stripped_line) {
                    e2e_violations.push(format!(
                        "{}:{}: forbidden fixed-count polling loop (Decision 21)",
                        file.display(),
                        idx + 1
                    ));
                }
            }
        }
    }

    // Bind every annotation to its test function with syn — the same
    // walker rule 7 runs (issue #406). A parse failure here is fatal:
    // with no reliable binding, checks 1/2/4/6 would report garbage.
    let docs_config = xtask_docs::DocsConfig::from_workspace(root.clone());
    let discovered = match discover_spec_tests(&docs_config) {
        Ok(tests) => tests,
        Err(e) => {
            eprintln!("failed to parse #[spec] test sources: {e}");
            return ExitCode::from(2);
        }
    };

    // Issue #406, the honest-failure half: every `#[spec(...)]` the text
    // scan found must correspond to a function syn bound. One that does
    // not (annotating a non-`fn` item, or emitted from inside a macro
    // body syn does not expand) is named at its own file:line rather
    // than silently attaching itself to a neighbouring function.
    failures.extend(unattached_annotation_failures(&occurrences, &discovered));

    let mut annotated_ids: BTreeSet<&str> = BTreeSet::new();
    for ann in &discovered {
        annotated_ids.insert(&ann.spec_id);

        // Check 2: annotation references a real catalog ID.
        if !catalog_ids.contains(&ann.spec_id) {
            failures.push(format!(
                "[2] {} carries #[spec({:?})] which is not in the catalog",
                ann.source_path.display(),
                ann.spec_id
            ));
        }

        // Check 4: function name carries a Decision-17 prefix derived
        // from the catalog ID. We accept EITHER the short `<sub>_<NNN>`
        // form OR the category-qualified `<area>_<sub>_<NNN>` full-ID
        // form (both hyphen → underscore normalized for Rust idents,
        // M2.1 reviewer S1). The qualified form is what lets tests whose
        // short prefix collides across categories carry unambiguous
        // names WITHOUT renaming — e.g. `chain-smoke/pi/001` and
        // `scheduler/pi/001` both shorten to `pi_001`, so they use
        // `chain_smoke_pi_001` / `scheduler_pi_001` (PRD #201). The short
        // form stays valid so the many pre-existing short-named tests —
        // including other colliding sub-areas that predate this rule
        // (`help_001`, `form_001`, `live_001`, `spawn_001`,
        // `selection_001`, `layout_001`, …) — keep passing. See
        // `fn_name_matches_spec`.
        if !fn_name_matches_spec(&ann.spec_id, &ann.fn_name) {
            failures.push(format!(
                "[4] {} fn `{}` does not start with `{}` (short) or `{}` (category-qualified) (Decision 17, derived from #[spec({:?})])",
                ann.source_path.display(),
                ann.fn_name,
                sub_area_prefix(&ann.spec_id).unwrap_or_default(),
                qualified_id_prefix(&ann.spec_id).unwrap_or_default(),
                ann.spec_id
            ));
        }

        // Check 6 (Decision 26): read straight off the function's own
        // attributes. The old line scan credited this test with any
        // `#[ignore]` sitting between the annotation and the next plain
        // `fn`, which could belong to a different function entirely.
        if ann.ignored {
            ignore_violations.push(format!(
                "{}: #[spec({:?})] annotates an #[ignore]-d test `{}` (Decision 26)",
                ann.source_path.display(),
                ann.spec_id,
                ann.fn_name
            ));
        }
    }

    // Check 1: every catalog ID has at least one annotation OR is on
    // the allowlist (M2 ships only `dashboard/pane/004` and
    // `hooks/delivery/001`; M4+ ticks IDs off the allowlist as it
    // lands tests).
    for id in &catalog_ids {
        if annotated_ids.contains(id.as_str()) {
            continue;
        }
        if allowlist.contains(id) {
            continue;
        }
        failures.push(format!(
            "[1] catalog ID `{id}` has no #[spec({id:?})]-annotated test and is not on the M2 allowlist"
        ));
    }

    failures.extend(e2e_violations);
    failures.extend(ignore_violations);
    failures.extend(
        bare_tempdir_violations
            .into_iter()
            .map(|v| format!("[8] {v}")),
    );

    failures.extend(
        unarmed_spawn_violations
            .into_iter()
            .map(|v| format!("[10] {v}")),
    );

    failures.extend(
        overlong_cap_violations
            .into_iter()
            .map(|v| format!("[11] {v}")),
    );

    // Check 9 (issue #474): `src/test_temp.rs` names no crate of its own. It is
    // read directly rather than folded into the scan above, so that the file
    // going missing is reported instead of quietly emptying the rule.
    failures.extend(
        check_self_contained(&root)
            .into_iter()
            .map(|v| format!("[9] {v}")),
    );

    // Check 7 (PRD #77 Decision 30 / M4.3): every #[spec] test has
    // a `/// Scenario:` doc comment with a body AND
    // `cargo xtask docs --tests` succeeds against the current source
    // + catalog. The xtask-docs library raises `Err` on a missing
    // Scenario or a malformed test source, which is exactly the two
    // failure modes we want to surface here. The byte-identity check
    // against on-disk `.md` is gone in M4.3: `.dot-agent-deck/` is
    // gitignored, so on a fresh clone there is no `.md` to compare.
    if let Err(e) = xtask_docs::check_rule_7(&docs_config) {
        failures.push(format!("[7] {e}"));
    }

    if failures.is_empty() {
        println!(
            "linkage-check: ok ({} catalog ids, {} annotations, {} allowlisted, 11 rules)",
            catalog_ids.len(),
            discovered.len(),
            allowlist.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("linkage-check: {} failure(s):", failures.len());
        for f in &failures {
            eprintln!("  {f}");
        }
        ExitCode::FAILURE
    }
}

/// `cargo xtask docs --tests` dispatch. Performs the same work as
/// the `xtask-docs` binary's main, in-process — we share the
/// library entry points so the two binaries stay in lockstep.
fn run_docs(args: &[String]) -> ExitCode {
    for arg in args {
        match arg.as_str() {
            "--tests" => {}
            "-h" | "--help" => {
                println!("usage: cargo xtask docs --tests");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask docs: unknown argument {other:?}");
                eprintln!("usage: cargo xtask docs --tests");
                return ExitCode::from(2);
            }
        }
    }
    let root = repo_root();
    let config = xtask_docs::DocsConfig::from_workspace(root.clone());
    let generated = match xtask_docs::generate_all(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let written = match xtask_docs::write_all(&generated) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    for path in &written {
        let rel = path.strip_prefix(&root).unwrap_or(path.as_path());
        println!("wrote {}", rel.display());
    }
    ExitCode::SUCCESS
}

/// `cargo xtask list-tests` dispatch (PRD #77 Decision 31). Emits a
/// Markdown synthetic-test inventory between the current branch and
/// `origin/main` on stdout. The orchestrator runs this before
/// delegating release.
fn run_list_tests(args: &[String]) -> ExitCode {
    if let Some(first) = args.first() {
        match first.as_str() {
            "-h" | "--help" => {
                println!("usage: cargo xtask list-tests");
                println!();
                println!("Emits a Markdown report of every #[spec] test created or");
                println!("modified in this branch versus origin/main, plus per-catalog");
                println!("prose diffs and any xtask/linkage-check/m2.allowlist changes.");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask list-tests: unknown argument {other:?}");
                eprintln!("usage: cargo xtask list-tests");
                return ExitCode::from(2);
            }
        }
    }
    let root = repo_root();
    match list_tests::run(&root) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask list-tests: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One `#[spec("…")]` attribute as *located by text scan* — where it is
/// written, not what it annotates. Deciding which function it belongs to
/// is syn's job (issue #406); this exists so an annotation syn does not
/// bind can be reported at its own line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecOccurrence {
    id: String,
    file: PathBuf,
    line: usize,
}

/// Find every `#[spec("…")]` in `lines` (already comment-stripped),
/// returning `(catalog id, 1-based line number)` in source order.
///
/// This deliberately does NOT look for a following `fn`. The old walker
/// did, with `^\s*fn\s+`, and scanned to end-of-file for a match — so an
/// `async fn` test was skipped and its annotation re-bound to whatever
/// plain `fn` came next, hundreds of lines away (issue #406).
fn scan_spec_occurrences(lines: &[&str], spec_re: &Regex) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = spec_re.captures(line) {
            out.push((caps.get(1).unwrap().as_str().to_string(), i + 1));
        }
    }
    out
}

/// Collect every `#[spec]` test under `tests/` and `src/` using the
/// generator's syn walker, so linkage-check and rule 7 can never
/// disagree about which functions exist (issue #406).
fn discover_spec_tests(
    config: &xtask_docs::DocsConfig,
) -> Result<Vec<xtask_docs::DiscoveredTest>, String> {
    let mut tests = xtask_docs::discover_tests(&config.tests_dir)?;
    // PRD #83: `#[spec]` tests also live in the library crate.
    tests.extend(xtask_docs::discover_tests(&config.src_dir)?);
    Ok(tests)
}

/// Report any `#[spec(...)]` occurrence that syn did not bind to a
/// function. Matching is per `(file, catalog id)` by COUNT: syn knows
/// the function name but not its line, and the same id may legitimately
/// be annotated on more than one test in a file, so an excess of text
/// occurrences over bound functions is the reliable signal. The message
/// carries every line the id appears on in that file, which is enough to
/// find the stray one.
fn unattached_annotation_failures(
    occurrences: &[SpecOccurrence],
    discovered: &[xtask_docs::DiscoveredTest],
) -> Vec<String> {
    let mut bound: BTreeMap<(&Path, &str), usize> = BTreeMap::new();
    for t in discovered {
        *bound
            .entry((t.source_path.as_path(), t.spec_id.as_str()))
            .or_insert(0) += 1;
    }
    let mut scanned: BTreeMap<(&Path, &str), Vec<usize>> = BTreeMap::new();
    for o in occurrences {
        scanned
            .entry((o.file.as_path(), o.id.as_str()))
            .or_default()
            .push(o.line);
    }

    let mut out = Vec::new();
    for ((file, id), lines) in scanned {
        let bound_count = bound.get(&(file, id)).copied().unwrap_or(0);
        if lines.len() <= bound_count {
            continue;
        }
        let unbound = lines.len() - bound_count;
        let where_ = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "[4] {} {unbound} of {} #[spec({id:?})] annotation(s) (line(s) {where_}) is not attached to a `fn` definition \
             — an attribute on a non-function item, or inside a macro body the parser does not expand",
            file.display(),
            lines.len(),
        ));
    }
    out
}

/// Locate the workspace root by walking up from the binary's
/// `current_dir()` until we see the workspace `Cargo.toml` (which has
/// a `[workspace]` block).
fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().expect("current_dir is readable");
    loop {
        let candidate = dir.join("Cargo.toml");
        if let Ok(s) = std::fs::read_to_string(&candidate)
            && s.contains("[workspace]")
        {
            return dir;
        }
        if !dir.pop() {
            panic!("could not locate workspace root from {dir:?}");
        }
    }
}

/// Parse `## Test Case Catalog` out of the PRD: extract every
/// occurrence of `##### <area>/<sub>/<NNN>` (the catalog entry header
/// form). The deliberate-skips table at the bottom uses table rows,
/// not headers, so it is excluded by construction.
fn parse_catalog_ids(catalog_path: &Path) -> std::io::Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(catalog_path)?;
    let mut in_catalog = false;
    let header_re = Regex::new(r"^#####\s+([a-z][a-z0-9-]*/[a-z][a-z0-9-]*/\d{3})\b")
        .expect("catalog header regex compiles");
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            in_catalog = line.starts_with("## Test Case Catalog");
            continue;
        }
        if !in_catalog {
            continue;
        }
        if let Some(caps) = header_re.captures(line) {
            ids.insert(caps.get(1).unwrap().as_str().to_string());
        }
    }
    Ok(ids)
}

fn read_allowlist(path: &Path) -> std::io::Result<BTreeSet<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e),
    };
    let mut set = BTreeSet::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        set.insert(line.to_string());
    }
    Ok(set)
}

fn collect_test_rs_files(tests_dir: &Path) -> Vec<PathBuf> {
    let mut out: BTreeMap<PathBuf, ()> = BTreeMap::new();
    visit(tests_dir, &mut out);
    out.into_keys().collect()
}

fn visit(dir: &Path, acc: &mut BTreeMap<PathBuf, ()>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            visit(&p, acc);
        } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs") {
            acc.insert(p, ());
        }
    }
}

/// Blank the CONTENTS of every string, raw-string and char literal in `src`,
/// replacing each content byte with a space and leaving the delimiters (and the
/// line count) alone.
///
/// Fed the output of [`strip_rust_comments`], so it never has to tell a `"`
/// inside a comment from a real one — those bytes are already spaces.
///
/// **Used by check 10 only, deliberately, and not folded into
/// [`strip_rust_comments`].** That function's other callers *depend* on seeing
/// inside literals: check 9's doc comment says so outright ("A `crate::` inside
/// a string literal is not exempt"), and check 8's bare-tempdir scan reads the
/// same view. Changing the shared stripper in place would silently widen those
/// two rules' blind spots to buy check 10 its fix.
///
/// Why check 10 wants it, in both directions. As a **false negative**: the rule
/// clears a whole file the moment its arming regex matches anywhere, so a test
/// that merely *quotes* `child_lifetime_bound::arm()` — the obvious shape for a
/// future case asserting this rule's own remedy text — would satisfy it while
/// building an unarmed registry elsewhere in the file. As a **false positive**:
/// a test asserting on a log or error message containing `run_daemon_with(` or
/// `AgentPtyRegistry::new(` would fail the build for a file that spawns nothing.
///
/// Shares [`strip_rust_comments`]'s lexing limits, which are acceptable for the
/// same reason: byte-string prefixes (`b"…"`) blank correctly because the `"`
/// still opens a literal, while a raw *byte* string (`br"…"`) is read as a plain
/// string because the `r` is not at a token boundary — so its backslashes are
/// treated as escapes. The failure mode there is a mis-blanked literal, never a
/// dropped line, and no such literal exists under `tests/`.
fn blank_string_literal_contents(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut raw_string_hashes: Option<usize> = None;
    // Content bytes become spaces; newlines stay newlines so per-line indexing
    // into the result still matches the raw source's line numbers.
    let blank = |c: char, out: &mut String| out.push(if c == '\n' { '\n' } else { ' ' });

    while i < bytes.len() {
        let c = bytes[i] as char;

        if let Some(needed_hashes) = raw_string_hashes {
            if c == '"' {
                let mut hashes_seen = 0usize;
                while hashes_seen < needed_hashes
                    && bytes.get(i + 1 + hashes_seen).copied() == Some(b'#')
                {
                    hashes_seen += 1;
                }
                if hashes_seen == needed_hashes {
                    out.push('"');
                    for _ in 0..hashes_seen {
                        out.push('#');
                    }
                    i += 1 + hashes_seen;
                    raw_string_hashes = None;
                    continue;
                }
            }
            blank(c, &mut out);
            i += 1;
            continue;
        }

        if in_string || in_char {
            // An escape is two bytes and cannot close the literal, so consume
            // both — otherwise `"\""` would end one byte early.
            if c == '\\' && i + 1 < bytes.len() {
                blank(c, &mut out);
                blank(bytes[i + 1] as char, &mut out);
                i += 2;
                continue;
            }
            if (in_string && c == '"') || (in_char && c == '\'') {
                out.push(c);
                in_string = false;
                in_char = false;
                i += 1;
                continue;
            }
            blank(c, &mut out);
            i += 1;
            continue;
        }

        // Raw string start: `r"`, `r#"`, `r##"`, … at a token boundary, so `for`
        // and `let_r` do not fire it.
        if c == 'r' {
            let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            let is_token_boundary = match prev {
                None => true,
                Some(b) => {
                    let pc = b as char;
                    !(pc.is_ascii_alphanumeric() || pc == '_')
                }
            };
            if is_token_boundary {
                let mut j = i + 1;
                while bytes.get(j).copied() == Some(b'#') {
                    j += 1;
                }
                if bytes.get(j).copied() == Some(b'"') {
                    let hashes = j - (i + 1);
                    out.push('r');
                    for _ in 0..hashes {
                        out.push('#');
                    }
                    out.push('"');
                    i = j + 1;
                    raw_string_hashes = Some(hashes);
                    continue;
                }
            }
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            // Same lifetime heuristic the comment stripper uses: `'a` followed
            // by something other than `'` is a lifetime, not a char literal.
            let next = bytes.get(i + 1).map(|b| *b as char);
            let after_after = bytes.get(i + 2).map(|b| *b as char);
            let looks_like_lifetime = next.is_some_and(|n| n.is_ascii_alphabetic() || n == '_')
                && after_after.is_some_and(|a| a != '\'');
            if !looks_like_lifetime {
                in_char = true;
            }
            out.push(c);
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Strip Rust `//` line comments and `/* … */` block comments from
/// `src`, replacing each stripped byte with a space. Line endings are
/// preserved 1-for-1 so per-line indexing into the stripped text
/// matches the raw source. String literals are honoured so a `//`
/// inside `"…"` is not mistakenly treated as a comment.
///
/// M4.6 P2: also recognises raw string literals (`r"…"`,
/// `r#"…"#`, `r##"…"##`, etc.). The closing delimiter is `"`
/// followed by exactly the same number of `#` characters that
/// opened the literal — an embedded `"` inside the body does NOT
/// close the string unless it has the matching hash suffix.
fn strip_rust_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut block_depth: usize = 0;
    // M4.6 P2: when inside a raw string literal, this holds the
    // number of `#` characters required between the closing `"` and
    // the end of the literal. `None` outside any raw string.
    let mut raw_string_hashes: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let next = bytes.get(i + 1).map(|b| *b as char);

        if let Some(needed_hashes) = raw_string_hashes {
            // Inside a raw string — content passes through verbatim;
            // only the matched `"` + `#…` sequence closes it. No
            // escape processing.
            out.push(c);
            if c == '"' {
                let mut hashes_seen = 0usize;
                while hashes_seen < needed_hashes
                    && bytes.get(i + 1 + hashes_seen).copied() == Some(b'#')
                {
                    hashes_seen += 1;
                }
                if hashes_seen == needed_hashes {
                    // Emit the trailing hashes verbatim and exit raw
                    // mode.
                    for _ in 0..hashes_seen {
                        out.push('#');
                    }
                    i += 1 + hashes_seen;
                    raw_string_hashes = None;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if block_depth > 0 {
            // Inside a block comment — only `*/` or nested `/*` matter;
            // newlines are preserved so line numbers align.
            if c == '/' && next == Some('*') {
                block_depth += 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '*' && next == Some('/') {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if in_char {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Raw string literal start: `r`, `r"`, or `r#…"`. The `r`
        // must be at a token boundary (previous byte is not an
        // identifier-continuation char) so the matcher doesn't fire
        // on `for`, `let_r`, etc.
        if c == 'r' {
            let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            let is_token_boundary = match prev {
                None => true,
                Some(b) => {
                    let pc = b as char;
                    !(pc.is_ascii_alphanumeric() || pc == '_')
                }
            };
            if is_token_boundary {
                let mut j = i + 1;
                while bytes.get(j).copied() == Some(b'#') {
                    j += 1;
                }
                if bytes.get(j).copied() == Some(b'"') {
                    let hashes = j - (i + 1);
                    // Emit the prefix verbatim: r + hashes + opening "
                    out.push('r');
                    for _ in 0..hashes {
                        out.push('#');
                    }
                    out.push('"');
                    i = j + 1;
                    raw_string_hashes = Some(hashes);
                    continue;
                }
            }
            // Fall through — `r` is just an identifier char.
        }

        if c == '/' && next == Some('/') {
            // Line comment — eat until newline (preserve the newline).
            while i < bytes.len() && bytes[i] as char != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            // Heuristic: only treat `'` as a char literal start when the
            // following byte is not an identifier continuation (lifetimes
            // look like `'a`). Comments inside lifetime annotations can't
            // exist anyway, so being conservative is fine.
            let after_after = bytes.get(i + 2).map(|b| *b as char);
            let looks_like_lifetime = next.is_some_and(|n| n.is_ascii_alphabetic() || n == '_')
                && after_after.is_some_and(|a| a != '\'');
            if !looks_like_lifetime {
                in_char = true;
            }
            out.push(c);
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Derive the Decision-17 `<sub>_<NNN>` prefix from a catalog ID,
/// applying the hyphen → underscore normalization used by Rust
/// identifiers (M2.1 reviewer S1). Returns `None` if the ID is not
/// of the expected three-segment shape.
fn sub_area_prefix(id: &str) -> Option<String> {
    let (rest, nnn) = id.rsplit_once('/')?;
    let (_area, sub) = rest.rsplit_once('/')?;
    Some(format!("{}_{nnn}", sub.replace('-', "_")))
}

/// Derive the category-qualified Decision-17 prefix from a catalog ID:
/// the FULL `<area>/<sub>/<NNN>` with `/` and `-` replaced by `_`
/// (e.g. `chain-smoke/pi/001` → `chain_smoke_pi_001`). This is the
/// unambiguous form used when a short `<sub>_<NNN>` prefix would collide
/// across categories. Returns `None` for malformed IDs (same
/// three-segment shape guard as [`sub_area_prefix`]).
fn qualified_id_prefix(id: &str) -> Option<String> {
    let (rest, nnn) = id.rsplit_once('/')?;
    let (area, sub) = rest.rsplit_once('/')?;
    Some(format!(
        "{}_{}_{nnn}",
        area.replace('-', "_"),
        sub.replace('-', "_")
    ))
}

/// Decision-17 acceptance: does `fname` carry a prefix traceable to
/// catalog `id`? Accepts EITHER the short `<sub>_<NNN>` form
/// ([`sub_area_prefix`]) OR the category-qualified `<area>_<sub>_<NNN>`
/// form ([`qualified_id_prefix`]).
///
/// Accepting both is deliberate: the qualified form disambiguates
/// cross-category same-sub-area IDs whose short prefixes collide
/// (`chain-smoke/pi/001` and `scheduler/pi/001` both shorten to
/// `pi_001`), while the short form keeps the many pre-existing
/// short-named tests valid — including other colliding sub-areas
/// (`help_001`, `form_001`, `live_001`, `spawn_001`, …) that predate
/// this rule. We do NOT reject the short form on collision: that would
/// force ~20 already-shipped tests to rename, which is out of scope and
/// contrary to the "keep existing short-form names valid" contract.
///
/// A malformed ID with no derivable prefix is treated as vacuously OK —
/// the ID-format check (check 3) already flags it.
fn fn_name_matches_spec(id: &str, fname: &str) -> bool {
    let short = sub_area_prefix(id).unwrap_or_default();
    let qualified = qualified_id_prefix(id).unwrap_or_default();
    if short.is_empty() && qualified.is_empty() {
        return true;
    }
    (!short.is_empty() && fname.starts_with(&short))
        || (!qualified.is_empty() && fname.starts_with(&qualified))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check 8 must see the *file* constructors, not only the directory ones.
    /// This is the hole that let the Codex-auth pre-flight's
    /// `NamedTempFile::new()` sit inside the rule's own scope, measured live in
    /// `/tmp` on `5e8e0ed`.
    #[test]
    fn bare_temp_ctor_re_matches_file_constructors() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let f = tempfile::NamedTempFile::new()",
            "    let f = NamedTempFile::new().unwrap();",
            "    let f = tempfile::tempfile().unwrap();",
            "    let f = tempfile::Builder::new().tempfile()?;",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    #[test]
    fn bare_temp_ctor_re_still_matches_dir_constructors() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let d = tempfile::tempdir().unwrap();",
            "    let d = TempDir::new().unwrap();",
            "    let d = tempfile::Builder::new().tempdir()?;",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    /// The `…_in` forms name their parent explicitly — they are what the
    /// wrappers themselves call, so matching them would make the rule
    /// unsatisfiable.
    #[test]
    fn bare_temp_ctor_re_allows_explicit_parent_forms() {
        let re = bare_temp_ctor_re();
        for line in [
            "    tempfile::Builder::new().tempdir_in(harness_temp_root())",
            "    tempfile::Builder::new().tempfile_in(harness_temp_root())",
            "    NamedTempFile::new_in(parent)?",
            "    TempDir::new_in(parent)?",
            "    tempfile::tempfile_in(parent)?",
            // The widened family's own `…_in` counterparts. `_in` sits between
            // the name and the `(`, which is what keeps these out.
            "    TempDir::with_prefix_in(\"codex-home-\", parent)?",
            "    TempDir::with_suffix_in(\".git\", parent)?",
            "    NamedTempFile::with_prefix_in(\"auth-\", parent)?",
            "    NamedTempFile::with_suffix_in(\".json\", parent)?",
            "    tempfile::spooled_tempfile_in(4096, parent)?",
        ] {
            assert!(!re.is_match(line), "should NOT be a violation: {line}");
        }
    }

    /// The `with_prefix` / `with_suffix` / `spooled` family allocates in
    /// `std::env::temp_dir()` exactly like `new()` does, and the rule claims to
    /// cover every bare constructor under `tests/`. These had no live call site
    /// when this test was written; it exists so the claim and the guard cannot
    /// drift apart again, which is how `NamedTempFile::new()` went unmatched.
    #[test]
    fn bare_temp_ctor_re_matches_the_prefix_suffix_and_spooled_family() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let d = TempDir::with_prefix(\"codex-home-\").unwrap();",
            "    let d = tempfile::TempDir::with_suffix(\"-repo\").unwrap();",
            "    let f = NamedTempFile::with_prefix(\"auth-\").unwrap();",
            "    let f = tempfile::NamedTempFile::with_suffix(\".json\").unwrap();",
            "    let f = tempfile::spooled_tempfile(4096);",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    /// Scope: **all** of `tests/` — the e2e tier, the harness, and the fast-tier
    /// crates that used to be excluded — plus `src/dispatch.rs`. The rest of
    /// `src/` stays out; that is the one remaining documented gap in
    /// `docs/develop/e2e-temp-dirs.md`.
    #[test]
    fn temp_ctor_rule_covers_the_documented_scope() {
        let root = Path::new("/repo");
        let tests_dir = root.join("tests");
        let covers = |rel: &str, is_e2e: bool| {
            temp_ctor_rule_covers(&root.join(rel), root, &tests_dir, is_e2e)
        };

        assert!(covers("tests/e2e_handshake.rs", true));
        assert!(covers("tests/common/mod.rs", false));
        assert!(covers("tests/daemon_protocol.rs", false));
        assert!(covers("src/dispatch.rs", false));

        // The six converted in the same commit that widened this rule, and a
        // name that does not exist yet — the whole point of a directory rule is
        // that a new file under `tests/` inherits it without being listed.
        assert!(covers("tests/rehydration.rs", false));
        assert!(covers("tests/pane_close.rs", false));
        assert!(covers("tests/codex_hooks_safety.rs", false));
        assert!(covers("tests/features.rs", false));
        assert!(covers("tests/devin_hook_ingestion.rs", false));
        assert!(covers("tests/codex_hook_ingestion.rs", false));
        assert!(covers("tests/some_future_suite.rs", false));

        // Still outside: everything in `src/` except the one listed file.
        assert!(!covers("src/config.rs", false));
        assert!(!covers("src/test_temp.rs", false));
    }

    /// Check 9 rejects a `crate::` path wherever it sits — a `use`, a call —
    /// and reports each at its own raw line number.
    #[test]
    fn self_contained_violations_rejects_crate_paths() {
        let src = concat!(
            "use std::io;\n",                           // 1
            "use crate::config::Config;\n",             // 2
            "\n",                                       // 3
            "pub fn tempdir() -> io::Result<()> {\n",   // 4
            "    let _ = crate::paths::state_dir();\n", // 5
            "    Ok(())\n",                             // 6
            "}\n",                                      // 7
        );

        let found = self_contained_violations("src/test_temp.rs", src);

        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found[0].starts_with("src/test_temp.rs:2: "), "{}", found[0]);
        assert!(found[1].starts_with("src/test_temp.rs:5: "), "{}", found[1]);
        // The diagnostic has to explain itself — N compile errors that do not
        // is the situation this rule exists to replace.
        assert!(found[0].contains("issue #474"), "{}", found[0]);
    }

    /// The two shapes that are easy to write without noticing: `macro_rules!`'s
    /// `$crate::`, which expands to the *defining* crate's root and is
    /// non-portable for the same reason, and the spaced `crate ::` the compiler
    /// accepts.
    #[test]
    fn crate_path_re_matches_the_macro_and_spaced_forms() {
        let re = crate_path_re();
        for line in [
            "        $crate::test_temp::PREFIX",
            "    let _ = crate :: features::experimental_enabled();",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    /// Ordinary self-contained content stays accepted: `std`, `libc`,
    /// `tempfile`, `super::` from the module's own `mod tests`, a crate whose
    /// name merely ENDS in `crate`, and — the part that has to keep working —
    /// comments that name the forbidden path, because the file's header note
    /// points at this rule and this rule's message quotes the path back.
    #[test]
    fn self_contained_violations_accepts_a_self_contained_module() {
        let src = concat!(
            "//! Enforced by linkage-check rule 9: no `crate::` path may appear\n",
            "//! below, because several test crates `#[path]`-include this file.\n",
            "use std::io;\n",
            "use std::path::PathBuf;\n",
            "use some_crate::helper;\n",
            "\n",
            "const PREFIX: &str = \"dad-unit-\";\n",
            "\n",
            "pub fn tempdir() -> io::Result<tempfile::TempDir> {\n",
            "    // Nothing below may reach for crate::something.\n",
            "    let uid = unsafe { libc::geteuid() };\n",
            "    let _ = (uid, PathBuf::new(), PREFIX, helper);\n",
            "    tempfile::Builder::new().prefix(PREFIX).tempdir()\n",
            "}\n",
            "\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    #[test]\n",
            "    fn allocates() {\n",
            "        let dir = super::tempdir().expect(\"allocate\");\n",
            "        assert!(dir.path().is_dir());\n",
            "    }\n",
            "}\n",
        );

        assert_eq!(
            self_contained_violations("src/test_temp.rs", src),
            Vec::<String>::new()
        );
    }

    /// The wrapper reads the guarded path under whatever root it is handed. A
    /// synthetic root, deliberately — the live checkout's copy is clean by
    /// construction, so pointing this at it would pass for a reason that has
    /// nothing to do with the rule working.
    #[test]
    fn check_self_contained_reads_the_guarded_file() {
        let root = tempfile::tempdir().expect("synthetic workspace root");
        std::fs::create_dir_all(root.path().join("src")).expect("create src/");
        let file = root.path().join("src").join("test_temp.rs");

        std::fs::write(
            &file,
            "use std::io;\npub fn tempdir() -> io::Result<()> {\n    Ok(())\n}\n",
        )
        .expect("write a self-contained module");
        assert_eq!(check_self_contained(root.path()), Vec::<String>::new());

        std::fs::write(&file, "use std::io;\nuse crate::config::Config;\n")
            .expect("write a violating module");
        let found = check_self_contained(root.path());
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("test_temp.rs:2: "), "{}", found[0]);
    }

    /// A rename that leaves `SELF_CONTAINED_PATH` behind fails loudly instead of
    /// passing vacuously. A guard that silently matches nothing while still
    /// printing `ok` is the same shape of invisible constraint issue #474 is
    /// about.
    #[test]
    fn check_self_contained_reports_the_guarded_file_going_missing() {
        let root = tempfile::tempdir().expect("synthetic workspace root");

        let found = check_self_contained(root.path());

        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].contains("cannot read the file check 9 guards"),
            "{}",
            found[0]
        );
        assert!(found[0].contains("issue #474"), "{}", found[0]);
    }

    // ---------------------------------------------------------------------
    // Check 10 (issue #668): the wrapped-child lifetime bound is armed.
    // ---------------------------------------------------------------------
    //
    // Both directions, against synthetic sources: a checkout that is clean by
    // construction proves nothing about a rule, and would still print `ok` on
    // the day it silently stopped matching.

    /// The unarmed direction: a file that builds a registry and never arms is
    /// reported, at the line the registry is built on.
    #[test]
    fn unarmed_agent_spawn_violations_reports_a_file_that_never_arms() {
        let src = concat!(
            "use dot_agent_deck::agent_pty::{AgentPtyRegistry, SpawnOptions};\n",
            "\n",
            "#[test]\n",
            "fn spawns_a_stand_in() {\n",
            "    let registry = Arc::new(AgentPtyRegistry::new());\n",
            "    registry.spawn_agent(SpawnOptions::default()).unwrap();\n",
            "}\n",
        );

        let found = unarmed_agent_spawn_violations("tests/synthetic.rs", src);

        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("tests/synthetic.rs:5: "),
            "{}",
            found[0]
        );
        // The message has to say what to DO — the gap this rule closes was
        // invisible precisely because nothing at the call site named it.
        assert!(found[0].contains("common::init_test_env()"), "{}", found[0]);
        assert!(
            found[0].contains("child_lifetime_bound::arm()"),
            "{}",
            found[0]
        );
        assert!(found[0].contains("issue #668"), "{}", found[0]);
    }

    /// The armed direction, both markers: the harness call for files that link
    /// `common`, and the standalone include for the ones that deliberately do
    /// not. Either clears the whole file, because arming happens once per
    /// process — usually in a helper hundreds of lines from the spawn it
    /// protects.
    #[test]
    fn unarmed_agent_spawn_violations_accepts_either_arming_marker() {
        let via_harness = concat!(
            "mod common;\n",
            "\n",
            "fn start() {\n",
            "    common::init_test_env();\n",
            "    let registry = Arc::new(AgentPtyRegistry::new());\n",
            "    let _ = run_daemon_with(&hook, daemon);\n",
            "}\n",
        );
        let via_include = concat!(
            "#[path = \"common/child_lifetime_bound.rs\"]\n",
            "mod child_lifetime_bound;\n",
            "\n",
            "async fn start_server() -> Server {\n",
            "    child_lifetime_bound::arm();\n",
            "    let registry = Arc::new(AgentPtyRegistry::new());\n",
            "}\n",
        );

        assert_eq!(
            unarmed_agent_spawn_violations("tests/harness.rs", via_harness),
            Vec::<String>::new()
        );
        assert_eq!(
            unarmed_agent_spawn_violations("tests/standalone.rs", via_include),
            Vec::<String>::new()
        );
    }

    /// `run_daemon_with` is the second trigger, and it needs its `(`: an import
    /// names the function without calling it, and `tests/rehydration.rs` has one
    /// at the top of a file that arms four helpers further down. A rule that
    /// fired on the `use` line would be reporting the wrong thing even when the
    /// file is correct.
    #[test]
    fn agent_spawn_site_re_matches_calls_not_imports() {
        let re = agent_spawn_site_re();
        for line in [
            "        let _ = run_daemon_with(&hook_for_task, daemon).await;",
            "    let registry = Arc::new(AgentPtyRegistry::new());",
            "    let registry = AgentPtyRegistry :: default ();",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
        for line in [
            "use dot_agent_deck::daemon::{Daemon, run_daemon_with};",
            "    registry: Arc<AgentPtyRegistry>,",
            "    let registry = daemon.pty_registry.clone();",
            "    let daemon = common::spawn_inprocess_daemon().await;",
        ] {
            assert!(!re.is_match(line), "should NOT be a violation: {line}");
        }
    }

    /// Prose is not a spawn site. Run over the comment-stripped view so this
    /// rule's own doc comments — and the module headers that explain the gap by
    /// naming `AgentPtyRegistry::new()` — are not violations of it. The same
    /// reason checks 5, 8 and 9 strip first.
    #[test]
    fn unarmed_agent_spawn_violations_ignores_comments() {
        let src = concat!(
            "//! The in-process `AgentPtyRegistry::new()` path was unarmed.\n",
            "// let registry = Arc::new(AgentPtyRegistry::new());\n",
            "/* run_daemon_with(&hook, daemon) is what the harness calls. */\n",
            "fn nothing() {}\n",
        );

        assert_eq!(
            unarmed_agent_spawn_violations("tests/prose.rs", src),
            Vec::<String>::new()
        );
    }

    /// An arming marker inside a STRING is not an arming call. Without the
    /// literal-blanking step this file passed: the regex saw
    /// `child_lifetime_bound::arm()` in the assertion text and cleared the real,
    /// unarmed registry below it. The shape is not hypothetical — it is what a
    /// test asserting this rule's own remedy wording looks like.
    #[test]
    fn unarmed_agent_spawn_violations_ignores_arming_markers_inside_string_literals() {
        let normal = concat!(
            "fn asserts_the_rule_text() {\n",
            "    assert!(msg.contains(\"child_lifetime_bound::arm()\"));\n",
            "    assert!(msg.contains(\"common::init_test_env()\"));\n",
            "}\n",
            "\n",
            "fn spawns_unarmed() {\n",
            "    let registry = Arc::new(AgentPtyRegistry::new());\n",
            "}\n",
        );
        let raw = concat!(
            "fn asserts_the_rule_text() {\n",
            "    let want = r#\"call common::init_test_env() before spawning\"#;\n",
            "}\n",
            "\n",
            "fn spawns_unarmed() {\n",
            "    let registry = Arc::new(AgentPtyRegistry::new());\n",
            "}\n",
        );

        let found = unarmed_agent_spawn_violations("tests/quotes_the_remedy.rs", normal);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("tests/quotes_the_remedy.rs:7: "),
            "{}",
            found[0]
        );

        let found_raw = unarmed_agent_spawn_violations("tests/quotes_the_remedy_raw.rs", raw);
        assert_eq!(found_raw.len(), 1, "{found_raw:#?}");
        assert!(
            found_raw[0].starts_with("tests/quotes_the_remedy_raw.rs:6: "),
            "{}",
            found_raw[0]
        );
    }

    /// The other direction: a spawn-SHAPED call inside a string is not a spawn.
    /// A test asserting on a log line or an error message that happens to
    /// contain `run_daemon_with(` or `AgentPtyRegistry::new(` spawns nothing,
    /// and failing the build for it would be a false positive with no escape
    /// but the opt-out marker.
    #[test]
    fn unarmed_agent_spawn_violations_ignores_spawn_shapes_inside_string_literals() {
        let src = concat!(
            "fn asserts_on_a_message() {\n",
            "    assert!(log.contains(\"run_daemon_with(&hook, daemon) failed\"));\n",
            "    let want = r\"AgentPtyRegistry::new() is not called here\";\n",
            "    let ch = '\\\"';\n",
            "}\n",
        );

        assert_eq!(
            unarmed_agent_spawn_violations("tests/message_assertions.rs", src),
            Vec::<String>::new()
        );
    }

    /// The blanking keeps the file's shape: same line count, delimiters intact,
    /// and code outside literals byte-identical. Line numbers in every rule-10
    /// diagnostic depend on the first of those.
    #[test]
    fn blank_string_literal_contents_preserves_lines_and_leaves_code_alone() {
        let src = concat!(
            // A multi-line literal: the REAL newline inside it has to survive,
            // or every line number after this point is wrong.
            "let a = \"one\ntwo\";\n",
            "let b = r#\"raw \"quoted\" body\"#;\n",
            "let c = 'x';\n",
            "let d: &'static str = \"tail\";\n",
            "call_me(1);\n",
        );
        let out = blank_string_literal_contents(src);

        assert_eq!(
            out.lines().count(),
            src.lines().count(),
            "line count moved:\n{out}"
        );
        assert!(out.contains("let a = \"   \n   \";"), "{out}");
        assert!(out.contains("let b = r#\"                 \"#;"), "{out}");
        assert!(out.contains("let c = ' ';"), "{out}");
        // A lifetime is not a char literal, so the code after it is untouched.
        assert!(out.contains("let d: &'static str = \"    \";"), "{out}");
        assert!(out.contains("call_me(1);"), "{out}");
    }

    /// An escaped quote does not close a literal one byte early — otherwise the
    /// blanker would fall out of the string and start blanking real code.
    #[test]
    fn blank_string_literal_contents_handles_escaped_quotes() {
        let src = "let s = \"a\\\"b\"; run_daemon_with(x);\n";
        let out = blank_string_literal_contents(src);

        assert!(out.contains("run_daemon_with(x);"), "{out}");
        assert!(out.contains("let s = \"    \";"), "{out}");
    }

    /// A file with neither trigger is untouched — the rule claims territory, not
    /// the whole directory.
    #[test]
    fn unarmed_agent_spawn_violations_ignores_files_that_spawn_nothing() {
        let src = "#[test]\nfn renders() {\n    assert_eq!(2 + 2, 4);\n}\n";

        assert_eq!(
            unarmed_agent_spawn_violations("tests/render_layout.rs", src),
            Vec::<String>::new()
        );
    }

    /// The escape hatch, on the offending line, the way check 8's is. No file
    /// needs it today; it exists so a deliberate exception (one pinning its own
    /// per-`Command` cap) is declared where it is taken instead of weakening the
    /// rule for everyone. Note it suppresses only the line it is on — the second
    /// site below is still reported.
    #[test]
    fn unarmed_agent_spawn_violations_honours_the_line_opt_out() {
        let src = concat!(
            "fn pins_its_own_cap() {\n",
            "    let registry = AgentPtyRegistry::new(); // linkage-check:allow-unarmed-agent-spawn\n",
            "    let other = AgentPtyRegistry::new();\n",
            "}\n",
        );

        let found = unarmed_agent_spawn_violations("tests/exception.rs", src);

        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("tests/exception.rs:3: "),
            "{}",
            found[0]
        );
    }

    // Check 11 (issue #679): no pinned wrapped-child lifetime cap may exceed
    // the number `clean-e2e-tmp`'s dead-owner deletion floor is derived from.

    /// The cap the rule is written against, so a future raise of
    /// `MAX_PINNED_ORPHAN_CAP_SECS` does not quietly turn these fixtures into
    /// no-ops. Every "over" fixture below is built from this, not from a
    /// hard-coded 901.
    const CAP: u64 = clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS;

    /// The shape that actually exists on `main` — `.with_env(VAR, "900")` on a
    /// `TuiDeck` builder. At the cap it stands; one second over it does not.
    #[test]
    fn overlong_lifetime_cap_reports_a_with_env_pin_above_the_cap() {
        let at_cap = format!(
            "fn dispatch_002() {{\n    TuiDeck::builder()\n        .with_env(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"{CAP}\")\n        .spawn();\n}}\n"
        );
        assert_eq!(
            overlong_lifetime_cap_violations("tests/e2e_dispatcher_mode.rs", &at_cap),
            Vec::<String>::new(),
            "a pin exactly at the cap is what the floor is derived for"
        );

        let over = at_cap.replace(&format!("\"{CAP}\""), &format!("\"{}\"", CAP + 1));
        let found = overlong_lifetime_cap_violations("tests/e2e_dispatcher_mode.rs", &over);

        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("tests/e2e_dispatcher_mode.rs:3: "),
            "{}",
            found[0]
        );
        assert!(
            found[0].contains(&format!("pinned at {}s", CAP + 1)),
            "the message names the offending value: {}",
            found[0]
        );
        assert!(
            found[0].contains("MAX_PINNED_ORPHAN_CAP_SECS"),
            "the message names the constant to raise: {}",
            found[0]
        );
    }

    /// `tests/wrap_io.rs` spells the pin across four lines. `\s*` has to span
    /// the newlines or the whole file goes unchecked, which is the shape most
    /// likely to be written next.
    #[test]
    fn overlong_lifetime_cap_spans_a_multi_line_env_call() {
        let src = format!(
            "fn run_wrap() {{\n    Command::new(bin)\n        .env(\n            \"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\",\n            \"{}\",\n        )\n        .spawn();\n}}\n",
            CAP + 100
        );

        let found = overlong_lifetime_cap_violations("tests/wrap_io.rs", &src);

        assert_eq!(found.len(), 1, "{found:#?}");
        // Reported at the line the VARIABLE is on, which is where a reader
        // looks, not at the value's line two below it.
        assert!(found[0].starts_with("tests/wrap_io.rs:4: "), "{}", found[0]);
    }

    /// The tuple and owned-`String` spellings `tests/common/mod.rs` uses at its
    /// two `env_clear` sites.
    #[test]
    fn overlong_lifetime_cap_reads_tuple_and_owned_pin_shapes() {
        let tuple = format!(
            "fn env() {{\n    let e = [(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"{}\")];\n}}\n",
            CAP + 1
        );
        let owned = format!(
            "fn env() {{\n    env.push((\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\".into(), \"{}\".into()));\n}}\n",
            CAP + 1
        );

        for src in [&tuple, &owned] {
            let found = overlong_lifetime_cap_violations("tests/common/mod.rs", src);
            assert_eq!(found.len(), 1, "{src}\n{found:#?}");
            assert!(
                found[0].starts_with("tests/common/mod.rs:2: "),
                "{}",
                found[0]
            );
        }
    }

    /// Shape 2. `tests/wrap_io.rs` passes `common::WRAP_TEST_MAX_LIFETIME_SECS`
    /// by name at nine sites, so the literal only ever appears at the `const`.
    /// A scan that read shape 1 alone would call that file clean while every
    /// one of those nine children carried the over-long cap.
    #[test]
    fn overlong_lifetime_cap_reads_a_constant_binding() {
        let over = format!(
            "pub const WRAP_TEST_MAX_LIFETIME_SECS: &str = \"{}\";\n",
            CAP + 1
        );
        let found = overlong_lifetime_cap_violations("tests/common/mod.rs", &over);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("tests/common/mod.rs:1: "),
            "{}",
            found[0]
        );

        // The unquoted `u64` spelling `child_lifetime_bound.rs` uses, too.
        let numeric = format!("const CHILD_MAX_LIFETIME_SECS: u64 = {};\n", CAP + 1);
        assert_eq!(
            overlong_lifetime_cap_violations("tests/common/child_lifetime_bound.rs", &numeric)
                .len(),
            1
        );
    }

    /// `tests/agent_lifetime_bound.rs` binds the variable's NAME to a constant
    /// whose own name matches the shape-2 pattern. Its value is a string of
    /// letters, and reading it as seconds would be a permanent false positive
    /// on a file that pins nothing.
    #[test]
    fn overlong_lifetime_cap_ignores_a_constant_holding_the_variable_name() {
        let src = concat!(
            "const MAX_LIFETIME_VAR: &str = \"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\";\n",
            "fn f() { command.env(MAX_LIFETIME_VAR, secs); }\n",
        );
        assert_eq!(
            overlong_lifetime_cap_violations("tests/agent_lifetime_bound.rs", src),
            Vec::<String>::new()
        );
    }

    /// Prose is not a pin. The doc comments explaining this very rule quote the
    /// over-long value that caused #679, and a scan that read comments would
    /// fail the build on its own documentation.
    #[test]
    fn overlong_lifetime_cap_ignores_comments() {
        let src = format!(
            "//! An exported `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS`, `{}`, was accepted.\n// (\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"{}\")\nfn f() {{}}\n",
            CAP + 2700,
            CAP + 2700
        );
        assert_eq!(
            overlong_lifetime_cap_violations("tests/prose.rs", &src),
            Vec::<String>::new()
        );
    }

    /// Every pin in one file is reported, ordered by line, and a file that pins
    /// nothing is silent.
    #[test]
    fn overlong_lifetime_cap_reports_every_site_in_order() {
        let src = format!(
            "fn a() {{ c.env(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"{}\"); }}\nfn b() {{ c.env(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"30\"); }}\nfn c() {{ c.env(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"{}\"); }}\n",
            CAP + 1,
            CAP + 2
        );

        let found = overlong_lifetime_cap_violations("tests/many.rs", &src);

        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found[0].starts_with("tests/many.rs:1: "), "{}", found[0]);
        assert!(found[1].starts_with("tests/many.rs:3: "), "{}", found[1]);

        assert_eq!(
            overlong_lifetime_cap_violations(
                "tests/render_layout.rs",
                "fn renders() { assert_eq!(1, 1); }\n"
            ),
            Vec::<String>::new()
        );
    }

    /// A literal too large for `u64` saturates rather than failing to parse, so
    /// the most over-long cap anyone could write is reported rather than
    /// silently skipped by the `?`-less parse path.
    #[test]
    fn overlong_lifetime_cap_reports_an_unparseable_giant() {
        let src = "fn f() { c.env(\"DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS\", \"99999999999999999999999\"); }\n";
        assert_eq!(
            overlong_lifetime_cap_violations("tests/giant.rs", src).len(),
            1
        );
    }

    /// The rule and the floor read the SAME constant. This is the assertion
    /// that makes rule 11 a guard rather than a second number to keep in sync:
    /// if `DEAD_PID_MIN_AGE` were ever re-hard-coded, the guard would still
    /// pass while the derivation it protects had drifted again.
    #[test]
    fn the_floor_is_derived_from_the_cap_the_rule_enforces() {
        assert_eq!(
            clean_tmp::DEAD_PID_MIN_AGE,
            std::time::Duration::from_secs(clean_tmp::MAX_PINNED_ORPHAN_CAP_SECS * 2),
            "linkage-check rule 11 bounds the caps under `tests/`; the reaper's \
             floor has to be derived from the same number or the guard bounds \
             nothing that matters (issue #679)"
        );
    }

    #[test]
    fn sub_area_prefix_handles_plain_sub_area() {
        assert_eq!(
            sub_area_prefix("dashboard/pane/004").as_deref(),
            Some("pane_004")
        );
    }

    #[test]
    fn sub_area_prefix_normalizes_hyphens_in_sub_area() {
        // PRD #77 catalog has these in the M2 allowlist; without the
        // hyphen → underscore normalization the function-name prefix
        // would be `pane-input_001_…` which is not a valid Rust ident.
        assert_eq!(
            sub_area_prefix("prompt/pane-input/001").as_deref(),
            Some("pane_input_001")
        );
        assert_eq!(
            sub_area_prefix("lifecycle/daemon-idle/002").as_deref(),
            Some("daemon_idle_002")
        );
        assert_eq!(
            sub_area_prefix("error/agent-spawn/001").as_deref(),
            Some("agent_spawn_001")
        );
    }

    #[test]
    fn sub_area_prefix_rejects_malformed_id() {
        assert_eq!(sub_area_prefix("not-an-id"), None);
        assert_eq!(sub_area_prefix("only/two"), None);
    }

    #[test]
    fn qualified_id_prefix_builds_full_id_form() {
        // Full-ID form: `/` and `-` → `_` across all three segments.
        assert_eq!(
            qualified_id_prefix("chain-smoke/pi/001").as_deref(),
            Some("chain_smoke_pi_001")
        );
        assert_eq!(
            qualified_id_prefix("scheduler/pi/001").as_deref(),
            Some("scheduler_pi_001")
        );
        assert_eq!(
            qualified_id_prefix("pi/live/002").as_deref(),
            Some("pi_live_002")
        );
        assert_eq!(
            qualified_id_prefix("dashboard/pane/004").as_deref(),
            Some("dashboard_pane_004")
        );
    }

    #[test]
    fn qualified_id_prefix_rejects_malformed_id() {
        assert_eq!(qualified_id_prefix("not-an-id"), None);
        assert_eq!(qualified_id_prefix("only/two"), None);
    }

    #[test]
    fn fn_name_matches_spec_accepts_short_form() {
        // Non-colliding sub-areas keep their short `<sub>_<NNN>` names.
        assert!(fn_name_matches_spec(
            "dashboard/pane/004",
            "pane_004_card_renders"
        ));
        // Colliding short forms that predate the qualified rule stay
        // valid via the short prefix (`help_001` is shared by three
        // catalog IDs, `live_002` by three, etc.).
        assert!(fn_name_matches_spec(
            "keybindings/help/001",
            "help_001_overlay"
        ));
        assert!(fn_name_matches_spec(
            "scheduler/live/002",
            "live_002_focusing_scheduled_card"
        ));
    }

    #[test]
    fn fn_name_matches_spec_accepts_category_qualified_form() {
        // PRD #201: the pi tests' short forms collide across categories
        // (`pi_001` from chain-smoke/pi + scheduler/pi), so they carry
        // category-qualified names — which must now be accepted WITHOUT
        // renaming them.
        assert!(fn_name_matches_spec(
            "chain-smoke/pi/001",
            "chain_smoke_pi_001_orchestrator_delegates_to_real_worker"
        ));
        assert!(fn_name_matches_spec(
            "scheduler/pi/001",
            "scheduler_pi_001_scheduled_unattended_status_via_extension"
        ));
        assert!(fn_name_matches_spec(
            "chain-smoke/pi/002",
            "chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done"
        ));
        assert!(fn_name_matches_spec(
            "pi/live/001",
            "pi_live_001_live_pane_shows_identity_and_status"
        ));
        assert!(fn_name_matches_spec(
            "pi/live/002",
            "pi_live_002_native_seeded_orchestration_delegates_live"
        ));
    }

    #[test]
    fn fn_name_matches_spec_rejects_unrelated_prefix() {
        // A name matching neither the short nor the qualified prefix is
        // still flagged.
        assert!(!fn_name_matches_spec(
            "chain-smoke/pi/001",
            "totally_unrelated_name"
        ));
        assert!(!fn_name_matches_spec(
            "dashboard/pane/004",
            "widget_004_something"
        ));
    }

    #[test]
    fn fn_name_matches_spec_vacuously_ok_for_malformed_id() {
        // Malformed IDs have no derivable prefix; check 3 flags the
        // format, so check 4 must not double-report.
        assert!(fn_name_matches_spec("not-an-id", "whatever_name"));
    }

    /// Build a `DiscoveredTest` standing in for one syn-bound test.
    fn bound(file: &str, spec_id: &str, fn_name: &str) -> xtask_docs::DiscoveredTest {
        xtask_docs::DiscoveredTest {
            spec_id: spec_id.to_string(),
            fn_name: fn_name.to_string(),
            source_path: PathBuf::from(file),
            scenario: Some("Scenario: synthetic.".to_string()),
            steps: Vec::new(),
            ignored: false,
        }
    }

    fn occurrence(file: &str, id: &str, line: usize) -> SpecOccurrence {
        SpecOccurrence {
            id: id.to_string(),
            file: PathBuf::from(file),
            line,
        }
    }

    #[test]
    fn scan_spec_occurrences_records_ids_and_line_numbers() {
        let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("regex");
        let lines = vec![
            "mod common;",
            r#"#[spec("hooks/delivery/001")]"#,
            "#[tokio::test]",
            "async fn delivery_001_async() {}",
            "",
            r#"#[spec("dashboard/pane/005")]"#,
            "#[test]",
            "fn pane_005_plain() {}",
        ];
        assert_eq!(
            scan_spec_occurrences(&lines, &spec_re),
            vec![
                ("hooks/delivery/001".to_string(), 2),
                ("dashboard/pane/005".to_string(), 6),
            ]
        );
    }

    #[test]
    fn scan_spec_occurrences_is_indifferent_to_what_follows() {
        // Issue #406: the scan no longer looks for a following `fn` at
        // all, so an `async fn` (or any other item shape) is recorded
        // identically. Binding is syn's job.
        let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("regex");
        let lines = vec![r#"#[spec("hooks/delivery/001")]"#, "async fn whatever() {}"];
        assert_eq!(
            scan_spec_occurrences(&lines, &spec_re),
            vec![("hooks/delivery/001".to_string(), 1)]
        );
    }

    #[test]
    fn unattached_annotations_are_silent_when_every_one_is_bound() {
        let occ = vec![
            occurrence("tests/a.rs", "hooks/delivery/001", 10),
            occurrence("tests/a.rs", "dashboard/pane/005", 20),
        ];
        let found = vec![
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x"),
            bound("tests/a.rs", "dashboard/pane/005", "pane_005_y"),
        ];
        assert!(unattached_annotation_failures(&occ, &found).is_empty());
    }

    #[test]
    fn unattached_annotation_is_reported_at_its_own_location() {
        // The honest-failure half of issue #406: an annotation syn could
        // not bind is named in ITS file, with its line — never silently
        // charged to a neighbouring function.
        let occ = vec![occurrence("tests/a.rs", "hooks/delivery/001", 42)];
        let failures = unattached_annotation_failures(&occ, &[]);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("tests/a.rs"), "{}", failures[0]);
        assert!(failures[0].contains("line(s) 42"), "{}", failures[0]);
        assert!(
            failures[0].contains("hooks/delivery/001"),
            "{}",
            failures[0]
        );
    }

    #[test]
    fn unattached_annotation_counts_duplicates_per_file_and_id() {
        // The same catalog ID may legitimately be annotated on more than
        // one test in a file, so the check compares COUNTS: two
        // occurrences with one bound fn means exactly one is stray.
        let occ = vec![
            occurrence("tests/a.rs", "hooks/delivery/001", 10),
            occurrence("tests/a.rs", "hooks/delivery/001", 30),
        ];
        let found = vec![bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x")];
        let failures = unattached_annotation_failures(&occ, &found);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("1 of 2"), "{}", failures[0]);
        assert!(failures[0].contains("line(s) 10, 30"), "{}", failures[0]);

        // Two occurrences, two bound fns → nothing to report.
        let found_both = vec![
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x"),
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_y"),
        ];
        assert!(unattached_annotation_failures(&occ, &found_both).is_empty());
    }

    #[test]
    fn unattached_annotation_does_not_match_across_files() {
        // A bound test in another file must not satisfy this file's
        // annotation.
        let occ = vec![occurrence("tests/a.rs", "hooks/delivery/001", 10)];
        let found = vec![bound("tests/b.rs", "hooks/delivery/001", "delivery_001_x")];
        let failures = unattached_annotation_failures(&occ, &found);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("tests/a.rs"), "{}", failures[0]);
    }

    #[test]
    fn strip_rust_comments_removes_line_comments() {
        let src = "fn foo() { /* keep this */ let x = 1; // and this\nlet y = 2;}";
        let out = strip_rust_comments(src);
        // The `// and this` content disappears; the `let y = 2;` survives.
        assert!(!out.contains("and this"));
        assert!(out.contains("let y = 2;"));
    }

    #[test]
    fn strip_rust_comments_preserves_string_literal_double_slashes() {
        let src = r#"let url = "https://example.com/path";"#;
        let out = strip_rust_comments(src);
        assert!(out.contains("https://example.com/path"));
    }

    #[test]
    fn strip_rust_comments_preserves_line_count() {
        let src = "// line1\nlet x = 0;\n// line3";
        let out = strip_rust_comments(src);
        // Three lines in → three lines out — the per-line indexing in
        // check 5/6 depends on this invariant.
        assert_eq!(out.lines().count(), src.lines().count());
    }

    #[test]
    fn strip_rust_comments_handles_raw_string_with_embedded_quote() {
        // M4.6 P2: a raw string can legally contain a bare `"`
        // because the closing delimiter is `"#`. The stripper must
        // not exit string mode on the embedded `"` and start
        // treating the rest of the file as bare code, which would
        // re-enable the line/block comment scanner and could strip
        // `// foo` text the author intended to keep.
        let src = r##"let s = r#"contains " and // not a comment"#; // real comment
let x = 1;"##;
        let out = strip_rust_comments(src);
        // The literal `// not a comment` inside the raw string must
        // survive (raw-string content passes through verbatim).
        assert!(
            out.contains("// not a comment"),
            "raw-string body should pass through verbatim: {out}"
        );
        // The trailing `// real comment` outside the raw string
        // must be stripped.
        assert!(
            !out.contains("real comment"),
            "real line comment after the raw string must be stripped: {out}"
        );
        // Code after the comment line is still present.
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn strip_rust_comments_handles_nested_hash_raw_string() {
        // `r##"…"##` requires TWO `#` after the closing `"`. An
        // embedded `"#` (one hash) must NOT terminate the literal.
        let src = r###"let s = r##"contains "# (single-hash) here // not a comment"##; // real
let y = 2;"###;
        let out = strip_rust_comments(src);
        assert!(
            out.contains("// not a comment"),
            "embedded `\"#` inside r##\"...\"## must not exit raw mode: {out}"
        );
        assert!(
            !out.contains("real"),
            "real comment outside the raw string must be stripped: {out}"
        );
        assert!(out.contains("let y = 2;"));
    }

    #[test]
    fn strip_rust_comments_does_not_misidentify_identifier_starting_with_r() {
        // `for` starts with `f`, not `r`, but `let r_value = "…"`
        // is the corner case: the bare `r` is an identifier prefix,
        // followed by `_value`. The stripper must not treat that
        // `r` as a raw-string opener (no `#` or `"` follows
        // immediately). Same for `for` (the `r` is not at a token
        // boundary).
        let src = r#"for r_value in 0..3 { let _ = r_value; }
// line comment after"#;
        let out = strip_rust_comments(src);
        // Identifiers preserved.
        assert!(out.contains("for r_value in 0..3"));
        assert!(out.contains("let _ = r_value;"));
        // The trailing line comment is still stripped.
        assert!(!out.contains("line comment after"));
    }
}
