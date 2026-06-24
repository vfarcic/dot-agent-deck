#![cfg(feature = "e2e")]

//! L2 PTY showcase for GitHub issue-dispatch (PRD #120): a fired
//! `issue_dispatch` task must surface its per-issue card LIVE on an
//! ALREADY-ATTACHED TUI — the user-visible payoff the headless
//! `scheduler/dispatch/001-009` family (run-now over the attach socket, no PTY)
//! can't observe, and the clip the demo reel narrates.
//!
//! Unlike `tests/e2e_issue_dispatch.rs` (headless `daemon serve`, asserts on the
//! daemon's agent registry / on-disk worktrees), this drives the real
//! `dot-agent-deck` binary inside an isolated PTY via the `TuiDeck` harness — the
//! same harness `scheduler/live/*` uses — so the assertion lands on the RENDERED
//! vt100 grid and a `full-stream.cast` is recorded for the reel.
//!
//! It composes both seams:
//!   - the OFFLINE GitHub seam from `e2e_issue_dispatch.rs`: a stub `gh` on PATH
//!     (`issue list` → canned JSON, `pr list` → `[]`, `repo clone` → `git clone`
//!     of a local one-commit fixture remote) so no network / real GitHub is hit;
//!   - the live-fire seam from `scheduler/live/*`: the lazily-spawned daemon
//!     inherits the deck's env (so `DOT_AGENT_DECK_SCHEDULES` is loaded), and the
//!     fire is triggered with the `RunNow` control message over the deck's attach
//!     socket (no real cron wait, no real LLM — the dispatched agent is `cat`).
//!
//! The dispatched issue is a single-agent card (the fixture remote carries no
//! `.dot-agent-deck.toml`, so the clone resolves `default_command = cat`). That
//! is deliberate: `spawn::spawn` only surfaces SINGLE-AGENT cards live to an
//! attached TUI — orchestration fires are rebuilt by the TUI's hydration path on
//! reconnect, not surfaced via a flat live `SessionStart` (see the comment at the
//! `surface_spawned_pane` call). So the single-agent dispatch is the path that
//! actually paints live, mirroring the proven `scheduler/live/001`.
//!
//! PRD #120's `experimental` flag gates only the creation UX, not the dispatch
//! BEHAVIOR, so a configured `issue_dispatch` task fires regardless of the flag
//! and this deck env carries no `DOT_AGENT_DECK_EXPERIMENTAL`.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

// ---------------------------------------------------------------------------
// Stub `gh` + local fixture-remote harness (the OFFLINE GitHub seam) — mirrors
// `tests/e2e_issue_dispatch.rs`, trimmed to the single-agent showcase path.
// ---------------------------------------------------------------------------

/// A synthetic `gh` that branches on argv, reading its canned data from files
/// under `$GHSTUB_DIR`, keyed by the `--repo owner/name` (with `/` → `_`):
///   - `issue list` → prints `<key>/issues.json`;
///   - `pr list --head agent/issue-<n>` → prints `<key>/pr-<n>.json` if present,
///     else `[]` (the showcase issue has no PR → not skipped);
///   - `repo clone <repo> <dest>` → `git clone <key>/remote <dest>` (the local
///     fixture remote becomes the clone's `origin`).
const GH_STUB_SCRIPT: &str = r#"#!/bin/sh
# Synthetic `gh` for PRD #120 issue_dispatch L2 tests — fully offline.
group="$1"
sub="$2"
shift 2 2>/dev/null || true

if [ "$group" = "repo" ] && [ "$sub" = "clone" ]; then
    repo="$1"
    dest="$2"
    key=$(printf '%s' "$repo" | tr '/' '_')
    exec git clone --quiet "$GHSTUB_DIR/$key/remote" "$dest"
fi

repo=""
head=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --repo) shift; repo="$1" ;;
        --head) shift; head="$1" ;;
        *) ;;
    esac
    shift
done
key=$(printf '%s' "$repo" | tr '/' '_')

if [ "$group" = "issue" ] && [ "$sub" = "list" ]; then
    cat "$GHSTUB_DIR/$key/issues.json"
    exit 0
fi

if [ "$group" = "pr" ] && [ "$sub" = "list" ]; then
    n=${head##*-}
    if [ -f "$GHSTUB_DIR/$key/pr-$n.json" ]; then
        cat "$GHSTUB_DIR/$key/pr-$n.json"
    else
        printf '[]\n'
    fi
    exit 0
fi

echo "gh stub: unhandled invocation: $group $sub $*" 1>&2
exit 1
"#;

/// Holds the stub `gh` + the per-repo fixture data dir, both rooted in a scratch
/// tempdir kept alive for the test's lifetime.
struct GhStub {
    _scratch: tempfile::TempDir,
    /// `$GHSTUB_DIR` — per-repo canned data lives under here.
    dir: PathBuf,
    /// Dir holding the `gh` script; prepended to the daemon's PATH.
    bindir: PathBuf,
}

impl GhStub {
    fn new() -> Self {
        let scratch = tempfile::tempdir().expect("gh stub scratch tempdir");
        let base = scratch.path().to_path_buf();
        let dir = base.join("ghstub");
        let bindir = base.join("bin");
        std::fs::create_dir_all(&dir).expect("create ghstub data dir");
        std::fs::create_dir_all(&bindir).expect("create ghstub bin dir");
        let gh = bindir.join("gh");
        std::fs::write(&gh, GH_STUB_SCRIPT).expect("write gh stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))
                .expect("chmod gh stub");
        }
        GhStub {
            _scratch: scratch,
            dir,
            bindir,
        }
    }

    fn key(repo: &str) -> String {
        repo.replace('/', "_")
    }

    fn repo_dir(&self, repo: &str) -> PathBuf {
        self.dir.join(Self::key(repo))
    }

    /// Create the fixture remote for `repo`: a real one-commit git repo with NO
    /// `.dot-agent-deck.toml`, so the dispatched clone opens a single-agent card
    /// (`default_command`) — the path that surfaces live to the attached TUI.
    fn add_plain_repo(&self, repo: &str) {
        let rd = self.repo_dir(repo);
        std::fs::create_dir_all(&rd).expect("create repo fixture dir");
        init_remote(&rd.join("remote"));
    }

    /// Set the canned `gh issue list` output for `repo` (issue numbers, in
    /// returned order).
    fn set_issues(&self, repo: &str, numbers: &[u64]) {
        let body = numbers
            .iter()
            .map(|n| format!("{{\"number\":{n}}}"))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            self.repo_dir(repo).join("issues.json"),
            format!("[{body}]\n"),
        )
        .expect("write issues.json");
    }

    /// PATH with the stub `gh` dir first, so the daemon's `gh` resolves here
    /// while `git` still comes from the real PATH.
    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bindir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn ghstub_dir(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }
}

/// Initialize a fixture remote: a real git repo with one commit (`README.md`,
/// no `.dot-agent-deck.toml`). Commit identity is pinned inline so the repo does
/// not depend on the host's global git config.
fn init_remote(remote: &Path) {
    std::fs::create_dir_all(remote).expect("create remote dir");
    run_git(remote, &["-c", "init.defaultBranch=main", "init", "-q"]);
    std::fs::write(remote.join("README.md"), "issue-dispatch fixture\n").expect("write README");
    run_git(remote, &["add", "-A"]);
    run_git(
        remote,
        &[
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

/// One `[[scheduled_tasks]]` block with an `[scheduled_tasks.issue_dispatch]`
/// sub-table. The cron never fires on its own (Jan 1 00:00) — the fire is driven
/// via `RunNow`.
fn dispatch_task(name: &str, working_dir: &str, prompt: &str, repo: &str) -> String {
    format!(
        "[[scheduled_tasks]]\n\
         name = \"{name}\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{working_dir}\"\n\
         prompt = \"{prompt}\"\n\
         enabled = true\n\
         \n\
         [scheduled_tasks.issue_dispatch]\n\
         repo = \"{repo}\"\n\
         max_per_run = 5\n\
         \n"
    )
}

/// Fire a registered task on the deck's own daemon via the `RunNow` control
/// message over the attach socket (the same path the in-TUI manager dialog and
/// `scheduler/live/*` use).
fn run_now(deck: &TuiDeck, name: &str) {
    common::attach_request_on(
        deck.attach_socket_path(),
        &dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        },
    )
    .unwrap_or_else(|e| panic!("RunNow {name} over the attach socket failed: {e}"));
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Scenario: Launch the deck attached to a daemon that has one enabled
/// `issue_dispatch` schedule (`github-issues`) targeting a stub-`gh` repo with a
/// single open issue (7) and no PR, then — without detaching — fire it via the
/// `RunNow` control message. The daemon clones the fixture remote, creates the
/// per-issue worktree `…/.worktrees/issue-7`, and spawns a single-agent `cat`
/// card into it. After confirming the daemon registered the dispatched agent
/// (precondition), assert that a per-issue card surfaces LIVE on the rendered
/// dashboard — its `Dir:` line shows the issue worktree basename `issue-7` and
/// its title shows the schedule name `github-issues` — proving the dispatched
/// issue is visible in the attached TUI (the showcase the demo reel narrates).
#[spec("scheduler/dispatch/011")]
#[test]
fn dispatch_011_card_surfaces_live_in_tui() {
    let stub = GhStub::new();
    let repo = "acme/widgets";
    stub.add_plain_repo(repo);
    stub.set_issues(repo, &[7]);

    // Workspace root where the clone (`<work>/github-issues`) and its per-issue
    // worktree are provisioned. A scratch tempdir, kept alive for the test.
    let work_td = tempfile::tempdir().expect("workspace tempdir");
    let work = work_td.path().join("ws");
    std::fs::create_dir_all(&work).expect("create workspace root");
    let work_str = work.to_string_lossy().into_owned();

    // A single-agent dispatch resolves its command from `default_command`; point
    // it at `cat` (long-lived, so the surfaced card persists) via a scratch
    // config the daemon reads through `DOT_AGENT_DECK_CONFIG`.
    let cfg_td = tempfile::tempdir().expect("config tempdir");
    let cfg = cfg_td.path().join("config.toml");
    std::fs::write(&cfg, "default_command = \"cat\"\n").expect("write config.toml");
    let cfg_str = cfg.to_string_lossy().into_owned();

    // The schedule fixture the lazily-spawned daemon loads via
    // `DOT_AGENT_DECK_SCHEDULES` (inherited from the deck's env).
    let sched_td = tempfile::tempdir().expect("schedules tempdir");
    let sched_path = sched_td.path().join("schedules.toml");
    std::fs::write(
        &sched_path,
        dispatch_task("github-issues", &work_str, "ISSUE-{{issue_number}}", repo),
    )
    .expect("write fixture schedules.toml");

    let path = stub.path_env();
    let ghdir = stub.ghstub_dir();
    let deck = TuiDeck::builder()
        .with_env("DOT_AGENT_DECK_SCHEDULES", sched_path.to_string_lossy())
        .with_env("PATH", path)
        .with_env("GHSTUB_DIR", ghdir)
        .with_env("DOT_AGENT_DECK_CONFIG", cfg_str)
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active sessions");

    // Fire the dispatch into the SAME daemon this TUI is attached to.
    run_now(&deck, "github-issues");

    // Precondition (daemon side): the dispatch flow clones, worktrees, and spawns
    // the per-issue agent under the schedule's friendly name. This isolates the
    // showcase below to the attached TUI's live surfacing — the registry holds
    // the agent regardless of whether the card paints.
    assert!(
        common::wait_for_agent_display_name(
            deck.attach_socket_path(),
            "github-issues",
            true,
            Duration::from_secs(20),
        ),
        "the daemon must clone + worktree + spawn the dispatched issue agent \
         (precondition for live surfacing)"
    );

    // The showcase: a card for the DISPATCHED ISSUE must appear LIVE on the
    // already-attached dashboard, with no disconnect/reconnect. The card's
    // `Dir:` line renders the spawn cwd basename — the per-issue worktree
    // `issue-7` — which is the per-issue identity (a plain scheduled card would
    // not carry it), so this is the load-bearing "a card for issue 7 surfaced"
    // signal.
    deck.wait_for_string("issue-7");

    // ...and the card is titled with the schedule's friendly name, confirming it
    // is the dispatch card (the whole card — title block + Dir body — paints in
    // one render pass, so once `issue-7` is on the grid the title is too).
    let grid = deck.snapshot_grid();
    assert!(
        grid.contains("github-issues"),
        "the live-surfaced dispatch card must be titled with the schedule name \
         'github-issues'.\nGrid:\n{grid}"
    );

    drop(work_td);
    drop(cfg_td);
    drop(sched_td);
}
