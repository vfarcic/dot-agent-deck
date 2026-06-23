#![cfg(feature = "e2e")]

//! L2 REAL-scenario showcase for GitHub issue-dispatch (PRD #120, CLAUDE.md
//! rule 4 real-scenario policy). This is the heaviest, flaky-tolerant pre-PR
//! tier: it drives the genuine `gh` → clone → per-issue worktree → real agent
//! path against LIVE GitHub with a REAL cheap-model (Haiku) agent — NO `gh`
//! stub, NO `cat` stand-in.
//!
//! Where `scheduler/dispatch/011` (in `e2e_issue_dispatch_live.rs`) composes an
//! OFFLINE `gh` stub with a `cat` card to prove the LIVE-SURFACING plumbing
//! deterministically, this test proves the thing a stand-in never can: that the
//! daemon really cloned a repo from GitHub and a real agent ran inside the
//! per-issue worktree and SAW the repo's files. The proof is a committed
//! sentinel filename (`DISPATCH_E2E_SENTINEL.md`) appearing in the dispatched
//! agent's rendered pane output.
//!
//! And per CLAUDE.md rule 4's "AS A USER ACTUALLY USES AND SEES IT" bar, the
//! agent here is a FULLY INTERACTIVE `claude` (not `claude -p`): the focused
//! card shows the genuine live claude TUI — its status (Thinking/Working/Bash)
//! and the Bash `ls` output streaming in — exactly what a user sees, not a
//! one-shot print piped to `cat`.
//!
//! Fixture (permanent, created out-of-band): the public repo
//! `vfarcic/dot-agent-deck-tests` carries a committed `DISPATCH_E2E_SENTINEL.md`,
//! a plain `.dot-agent-deck.toml` with NO `[[orchestrations]]` (so the dispatch
//! opens a single-agent card → it live-surfaces, like `dispatch/011`), and a
//! PERMANENT open issue #1 labelled `agent-dispatch-test`. The schedule filters
//! on that label with `max_per_run = 1`, so ONLY issue #1 is ever enumerated —
//! the run is deterministic against a fixed remote.
//!
//! Seams it exercises end to end:
//!   - REAL `gh` on the normal PATH (no stub): the lazily-spawned daemon really
//!     enumerates (`gh issue list`), checks for an in-flight PR (`gh pr list`),
//!     and clones (`gh repo clone`) against live GitHub. Auth is threaded via
//!     `GITHUB_TOKEN` (this environment's `gh` auth) — the harness scrubs the
//!     spawned deck's env to a pinned set, so the token is passed explicitly so
//!     the daemon and the `gh` it runs inherit it.
//!   - REAL cheap agent: `default_command` (via a `DOT_AGENT_DECK_CONFIG` scratch
//!     config, the same mechanism `dispatch/011` used for `cat`) launches a
//!     FULLY INTERACTIVE `claude` pinned to Haiku — the proven
//!     `delegate_work_done_chain_claude` launch mechanism + model id. NO `-p`
//!     and NO trailing `; cat`: interactive claude keeps its own pane alive and
//!     auto-submits the list-files directive the dispatch injects through its
//!     NORMAL prompt-delivery primitive (`write_to_pane_and_submit`). The deck
//!     builder seeds project-trust into the per-test HOME (via
//!     `with_claude_project_trust`) for the exact per-issue worktree cwd, so
//!     claude's first-run onboarding + trust gates clear with no human keystroke.
//!   - The live-fire seam from `scheduler/live/*`: the fire is driven by `RunNow`
//!     over the deck's attach socket (no real cron wait).
//!
//! PRD #120 ships `issue_dispatch` behind the `experimental` flag, so the deck
//! env sets `DOT_AGENT_DECK_EXPERIMENTAL=1`.
//!
//! NO REMOTE WRITES: the dispatch creates the branch `agent/issue-1` only in the
//! local tempdir clone and never pushes; the prompt is a list-files directive, so
//! the agent must not push or open a PR. The test asserts (best-effort) that the
//! fixture repo has no `agent/issue-1` branch after the run. All on-disk state
//! (clone + worktree) lives under a `tempfile::tempdir()` removed on drop.
//!
//! Decision 23 cost: one short interactive Haiku turn listing a handful of files
//! — well under the <$0.05/run bound. Local-only (Decision 8 / rule 5): gated on
//! the `e2e` feature so CI's `cargo test-fast` never compiles it; flaky-tolerant
//! (real LLM + real network) per rule 4 — it is NOT looped for flakiness.

mod common;

use std::time::Duration;

use common::TuiDeck;
use spec::spec;

const FIXTURE_REPO: &str = "vfarcic/dot-agent-deck-tests";
const FIXTURE_LABEL: &str = "agent-dispatch-test";
const SCHEDULE_NAME: &str = "github-issues";
const SENTINEL: &str = "DISPATCH_E2E_SENTINEL.md";
const PINNED_MODEL: &str = "claude-haiku-4-5-20251001";

/// The schedule's `[[scheduled_tasks]]` + `[scheduled_tasks.issue_dispatch]`
/// fixture the lazily-spawned daemon loads via `DOT_AGENT_DECK_SCHEDULES`. The
/// cron never fires on its own (Jan 1 00:00) — the fire is driven by `RunNow`.
/// `label` + `max_per_run = 1` pin enumeration to the single permanent issue #1.
fn dispatch_schedule_toml(working_dir: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{SCHEDULE_NAME}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         prompt = \"Use the Bash tool to run ls -a in the current directory, then print the complete file listing verbatim, one filename per line, with no other commentary.\"\n\
         enabled = true\n\
         \n\
         [scheduled_tasks.issue_dispatch]\n\
         repo = \"{FIXTURE_REPO}\"\n\
         label = \"{FIXTURE_LABEL}\"\n\
         max_per_run = 1\n\
         \n"
    )
}

/// The real-agent `default_command` for the dispatched single-agent card.
///
/// A FULLY INTERACTIVE `claude` (the proven `delegate_work_done_chain_claude`
/// launch mechanism) pinned to Haiku, with the Bash tool pre-allowed. NO `-p`
/// (so the user sees the genuine live claude TUI working in the pane — the
/// real user-visible experience, not a one-shot print) and NO trailing `; cat`
/// (interactive claude keeps the pane alive on its own). NO
/// `--dangerously-skip-permissions` (claude refuses it under root); `--allowedTools
/// Bash` is enough for the directive, which lands via the dispatch's NORMAL
/// prompt injection (interactive claude auto-submits it — the same primitive
/// the delegate dispatch uses).
///
/// Interactive claude shows a first-run trust dialog in a fresh worktree cwd, so
/// the deck builder seeds project-trust into the per-test HOME via
/// `with_claude_project_trust` for the exact per-issue worktree the agent runs
/// in — without that, the dialog would swallow the injected prompt.
///
/// No backticks (the daemon shell-wraps this under `/bin/sh -c`, and a backtick
/// would trigger command substitution).
fn real_agent_default_command() -> String {
    format!("claude --model {PINNED_MODEL} --allowedTools Bash")
}

/// Fire a registered task on the deck's own daemon via the `RunNow` control
/// message over the attach socket (mirrors `dispatch/011` / `scheduler/live/*`).
fn run_now(deck: &TuiDeck, name: &str) {
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        },
    )
    .unwrap_or_else(|e| panic!("RunNow {name} over the attach socket failed: {e}"));
}

/// Runtime-skip helper (Decision 26): this environment authenticates `gh` via
/// the `GITHUB_TOKEN` env var. Absent → the daemon's `gh` cannot enumerate/clone,
/// so the test is environmentally inapplicable rather than broken.
fn github_token() -> Result<String, String> {
    match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(t),
        _ => Err("GITHUB_TOKEN not set — real-GitHub issue-dispatch e2e needs gh auth".into()),
    }
}

/// Best-effort probe of the LIVE fixture remote: does the per-issue branch
/// `agent/issue-1` exist on GitHub? `Some(true)` = present (a remote write
/// leaked — a failure), `Some(false)` = confirmed absent, `None` = the `gh`
/// probe itself failed (network/transient) so we can't conclude either way.
/// Runs `gh` from the TEST process, which inherits the host's real env + auth.
fn remote_branch_exists(branch: &str) -> Option<bool> {
    let out = std::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/{FIXTURE_REPO}/branches/{branch}"),
            "--silent",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        return Some(true);
    }
    // gh exits non-zero for a 404 (branch absent) — distinguish that from a
    // network/auth failure by inspecting stderr for the GitHub "Not Found".
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Not Found") || stderr.contains("404") {
        Some(false)
    } else {
        None
    }
}

/// Scenario: Launch the deck attached to a daemon configured with one enabled
/// `issue_dispatch` schedule that targets the LIVE public fixture repo
/// `vfarcic/dot-agent-deck-tests` (filtered to the permanent label-gated issue
/// #1, `max_per_run = 1`) whose single-agent `default_command` is a FULLY
/// INTERACTIVE Haiku-pinned `claude` (no `-p`, no `; cat`), then fire it via
/// `RunNow` over the attach socket WITHOUT detaching. With the REAL `gh` on PATH
/// and `GITHUB_TOKEN` threaded through, the daemon enumerates, clones from
/// GitHub, creates the per-issue worktree `…/.worktrees/issue-1`, and spawns the
/// interactive agent into it — auto-submitting the injected list-files directive
/// (the per-issue worktree cwd was pre-trusted in the per-test HOME so claude's
/// first-run gates clear without a keystroke). After the dispatched agent
/// registers under the schedule name and its per-issue card surfaces live, focus
/// the card — now showing the genuine live claude TUI + status — and assert the
/// committed sentinel filename `DISPATCH_E2E_SENTINEL.md` appears in the agent's
/// rendered pane output, proving the daemon really cloned the repo and a real
/// interactive agent ran in the per-issue worktree and listed the repo files.
/// Finally assert (best-effort) the fixture repo has no pushed `agent/issue-1`
/// branch. Reel-eligible (PTY-attached, records a `full-stream.cast`);
/// flaky-tolerant (real LLM + real network) — run once, not looped.
#[spec("scheduler/dispatch/013")]
#[test]
fn dispatch_013_real_agent_lists_repo_files_from_github() {
    // Decision 26 runtime-skip: a missing CLI / credentials / token is an
    // environmental condition, not a broken test.
    skip_unless!(common::check_claude_available());
    let token = match github_token() {
        Ok(t) => t,
        Err(reason) => {
            // Same shape as `skip_unless!`'s early return (print `SKIP:` +
            // return) for the token precondition.
            eprintln!("SKIP: {reason}");
            return;
        }
    };

    // Workspace root where the daemon provisions the clone
    // (`<work>/github-issues`) and its per-issue worktree
    // (`<work>/github-issues/.worktrees/issue-1`). A scratch tempdir removed on
    // drop, so no clone/worktree leaks past the test.
    let work_td = tempfile::tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // The single-agent dispatch resolves its command from `default_command`;
    // point it at the real Haiku agent via a scratch config the daemon reads
    // through `DOT_AGENT_DECK_CONFIG` (same wiring `dispatch/011` used for `cat`).
    let cfg_td = tempfile::tempdir().expect("config tempdir");
    let cfg = cfg_td.path().join("config.toml");
    std::fs::write(
        &cfg,
        format!("default_command = \"{}\"\n", real_agent_default_command()),
    )
    .expect("write config.toml");
    let cfg_str = cfg.to_string_lossy().into_owned();

    // The schedule fixture the lazily-spawned daemon loads via
    // `DOT_AGENT_DECK_SCHEDULES` (inherited from the deck's env).
    let sched_td = tempfile::tempdir().expect("schedules tempdir");
    let sched_path = sched_td.path().join("schedules.toml");
    std::fs::write(&sched_path, dispatch_schedule_toml(&work_str)).expect("write schedules.toml");

    // The EXACT per-issue worktree cwd the dispatched interactive `claude` will
    // run in. Derived the same way the daemon does: `std::path::absolute` of the
    // workspace root (the daemon's `canonical_workspace` — lexical, no symlink
    // resolution) → `derive_issue_paths(.., SCHEDULE_NAME, 1)`. Seeding trust for
    // this exact path lets interactive claude clear its first-run trust dialog so
    // the injected list-files prompt isn't swallowed. Seeded BEFORE the dispatch
    // fires (the builder writes it into the per-test HOME at launch).
    let workspace_abs = std::path::absolute(&work).expect("absolutize workspace root");
    let worktree_cwd =
        dot_agent_deck::issue_dispatch::derive_issue_paths(&workspace_abs, SCHEDULE_NAME, 1)
            .worktree_dir
            .to_string_lossy()
            .into_owned();

    let deck = TuiDeck::builder()
        // Real Claude credentials so the daemon-spawned interactive `claude`
        // authenticates.
        .with_imported_claude_credentials()
        // Pre-trust the per-issue worktree cwd so interactive claude clears its
        // first-run onboarding + trust gates and auto-submits the injected prompt.
        .with_claude_project_trust(worktree_cwd)
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_CONFIG", cfg_str)
        // The harness scrubs the spawned deck's env to a pinned set, so thread
        // GITHUB_TOKEN explicitly — the daemon and the real `gh` it runs inherit
        // it for enumerate/clone against live GitHub.
        .with_env("GITHUB_TOKEN", token)
        // PRD #120 ships issue_dispatch behind the experimental flag; turn it ON.
        .with_env("DOT_AGENT_DECK_EXPERIMENTAL", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the dispatch into the SAME daemon this TUI is attached to.
    run_now(&deck, SCHEDULE_NAME);

    // Precondition (daemon side): the dispatch flow really enumerated, cloned
    // from GitHub, created the worktree, and spawned the per-issue agent under
    // the schedule's friendly name. A generous timeout absorbs the live clone +
    // `gh` round-trips.
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            SCHEDULE_NAME,
            true,
            Duration::from_secs(120),
        ),
        "the daemon must clone from GitHub + worktree + spawn the dispatched issue agent \
         under the schedule name '{SCHEDULE_NAME}' (precondition for the showcase)"
    );

    // The per-issue card surfaces LIVE on the already-attached dashboard; its
    // `Dir:` line shows the per-issue worktree basename `issue-1` (the per-issue
    // identity, proving the worktree was created).
    deck.wait_for_string("issue-1");

    // Focus the (only) dispatched card so its embedded agent PTY renders to the
    // grid (a dashboard card shows only summary metadata — the agent's stdout is
    // visible only once the pane is focused). Confirm focus landed.
    deck.send_keys(b"1");
    deck.wait_for_string("PaneInput mode");

    // The load-bearing showcase assertion: the committed sentinel filename must
    // appear in the dispatched agent's rendered pane output — only possible if
    // the daemon really cloned `vfarcic/dot-agent-deck-tests` from GitHub and the
    // real Haiku agent ran inside the per-issue worktree and listed its files.
    //
    // Scanned over the cumulative byte history (not a single grid frame): a live
    // interactive `claude` redraws/clears its screen constantly, but every byte
    // it ever emitted while the pane was focused stays in history. Generous
    // timeout absorbs the real LLM round-trip. Assert on the SENTINEL FILENAME,
    // not on exact agent phrasing.
    let saw_sentinel = deck.wait_for_stream_string_within(SENTINEL, Duration::from_secs(150));
    assert!(
        saw_sentinel,
        "the dispatched real agent's pane never showed the committed sentinel filename \
         {SENTINEL:?} within 150s — expected it to have cloned {FIXTURE_REPO} and listed the \
         per-issue worktree's files.\nFinal grid:\n{}",
        deck.snapshot_grid()
    );

    // NO REMOTE WRITES: a list-files directive must not push a branch or open a
    // PR. Confirm (best-effort) the fixture remote has no `agent/issue-1` branch.
    // `None` (the gh probe itself failed) is tolerated — only a definitive
    // present branch is a failure.
    if let Some(true) = remote_branch_exists("agent/issue-1") {
        panic!(
            "the fixture repo {FIXTURE_REPO} has a pushed branch 'agent/issue-1' after the run — \
             the dispatch/agent must NOT push (it operates only in the local tempdir clone)"
        );
    }

    drop(work_td);
    drop(cfg_td);
    drop(sched_td);
}
