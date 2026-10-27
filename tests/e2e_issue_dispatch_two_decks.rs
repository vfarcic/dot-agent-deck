#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Live GitHub claim gating between two isolated local decks. Agents are cat
//! stand-ins; GitHub enumeration, cloning, claims and readbacks are genuine.
//! Serialize runs sharing the permanent issue, including on one host. Immediate
//! reruns need the same account and host; foreign records retain their TTL.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use dot_agent_deck::issue_dispatch::{
    ClaimIdentity, IssueClaimRecord, claim_host_id, parse_issue_claim_records,
};
use serde_json::Value;
use spec::spec;

const REPO: &str = "vfarcic/dot-agent-deck-tests";
const LABEL: &str = "agent-dispatch-two-deck-test";
const ISSUE: u64 = 2;
// Stable names let the same account on this host recover A's own dead claim.
const SCHEDULE_A: &str = "scheduler_dispatch_041_a";
const SCHEDULE_B: &str = "scheduler_dispatch_041_b";

fn github_preflight() -> Result<String, String> {
    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or("GITHUB_TOKEN not set — two-deck claim e2e needs live GitHub auth")?;
    common::register_diagnostic_redactions([token.clone()]);
    match Command::new("gh")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(token),
        _ => Err("gh is unavailable — two-deck claim e2e needs the real GitHub CLI".into()),
    }
}

struct Github<'a> {
    token: &'a str,
    home: PathBuf,
    config: PathBuf,
    path: std::ffi::OsString,
    executable: PathBuf,
}

impl<'a> Github<'a> {
    fn new(root: &Path, token: &'a str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var_os("PATH").expect("PATH for the genuine gh");
        let executable = std::env::split_paths(&path)
            .map(|dir| dir.join("gh"))
            .find(|candidate| {
                candidate
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
            .and_then(|path| path.canonicalize().ok())
            .expect("resolve the genuine gh before installing agent stand-ins");
        let home = root.join("github-home");
        let config = home.join("gh-config");
        std::fs::create_dir_all(&config).expect("isolated gh HOME and config");
        Self {
            token,
            home,
            config,
            path,
            executable,
        }
    }

    fn read(&self, args: &[&str]) -> Value {
        // Match the daemons' token-only identity. In particular, ambient
        // GH_TOKEN, enterprise tokens, host overrides and gh config cannot win.
        let output = Command::new(&self.executable)
            .env_clear()
            .env("HOME", &self.home)
            .env("GH_CONFIG_DIR", &self.config)
            .env("PATH", &self.path)
            .env("GITHUB_TOKEN", self.token)
            .env("GH_HOST", "github.com")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run live gh readback");
        assert!(
            output.status.success(),
            "live gh readback {args:?} failed: {}",
            common::redact_credentials_for_output(&String::from_utf8_lossy(&output.stderr))
        );
        serde_json::from_slice(&output.stdout).expect("live GitHub JSON readback")
    }

    fn comments(&self) -> Vec<Value> {
        self.read(&[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{REPO}/issues/{ISSUE}/comments?per_page=100"),
        ])
        .as_array()
        .expect("comment pages")
        .iter()
        .flat_map(|page| page.as_array().expect("comment page").iter().cloned())
        .collect()
    }
}

fn own_record(comments: &[Value], me: &ClaimIdentity<'_>) -> Option<IssueClaimRecord> {
    parse_issue_claim_records(&serde_json::to_string(comments).unwrap())
        .expect("parse live claims with the production parser")
        .into_iter()
        .find(|record| record.is_own(me))
}

fn machine_digest() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for the length passed to gethostname, as in the daemon.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let hostname = String::from_utf8_lossy(&buf[..end]);
    let hostname = hostname.trim();
    claim_host_id(if rc == 0 && !hostname.is_empty() {
        hostname
    } else {
        "unknown-host"
    })
}

fn log(daemon: &common::DaemonProc) -> String {
    let path = daemon.home.parent().unwrap().join("daemon-stderr.log");
    common::redact_credentials_for_output(&std::fs::read_to_string(path).unwrap_or_default())
}

fn schedule(name: &str, workspace: &Path) -> String {
    format!(
        r#"[[scheduled_tasks]]
name = "{name}"
cron = "0 * * * *"
working_dir = {workspace}
prompt = "TWO-DECK-CLAIM-{{{{issue_number}}}}"
enabled = true

[scheduled_tasks.issue_dispatch]
repo = "{REPO}"
label = "{LABEL}"
max_per_run = 1
claim_gating = true
claim_ttl_hours = 3
"#,
        workspace = serde_json::to_string(&workspace.to_string_lossy()).unwrap(),
    )
}

/// Shadow the fixture repo's Claude command without changing its remote config.
/// A launch ledger detects even a short-lived agent on B, beyond ListAgents.
fn stand_in_path(root: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("stand-in-bin");
    std::fs::create_dir(&bin).expect("create stand-in bin");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf '2.1.296 (Claude Code)\n'
    exit 0
fi
printf '%s\n' "$$" >> "$TWO_DECK_AGENT_STARTS"
exec /bin/cat
"#,
    )
    .expect("write local stand-in");
    std::fs::set_permissions(claude, std::fs::Permissions::from_mode(0o755))
        .expect("make stand-in executable");
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn spawn_deck(
    root: &Path,
    name: &str,
    path: &str,
    token: &str,
) -> (common::DaemonProc, PathBuf, PathBuf) {
    let workspace = root.join(name);
    std::fs::create_dir(&workspace).expect("create deck workspace");
    let starts = workspace.join("agent-starts");
    let daemon = common::spawn_daemon_serve_with_env(
        Some(&schedule(name, &workspace)),
        "0",
        &[
            ("PATH", path),
            ("GITHUB_TOKEN", token),
            ("TWO_DECK_AGENT_STARTS", starts.to_str().unwrap()),
            ("GIT_AUTHOR_NAME", "Two Deck Test"),
            ("GIT_AUTHOR_EMAIL", "two-deck-test@example.invalid"),
            ("GIT_COMMITTER_NAME", "Two Deck Test"),
            ("GIT_COMMITTER_EMAIL", "two-deck-test@example.invalid"),
        ],
    );
    (daemon, workspace, starts)
}

fn fire_and_wait(daemon: &common::DaemonProc, name: &str, outcome: &str) {
    let reply = daemon
        .run_now(name)
        .expect("RunNow over isolated attach socket");
    assert!(reply.ok, "RunNow rejected: {:?}", reply.error);
    assert!(
        daemon.wait_for_stderr_contains(outcome, common::load_scaled(Duration::from_secs(90))),
        "schedule {name} must report {outcome:?}; daemon log:\n{}",
        log(daemon)
    );
}

/// Scenario: Fire deck A against permanent live GitHub issue #2, then fire isolated deck B with a different schedule and claim gating enabled.
/// A dispatches a local stand-in and writes its claim; B reports A as the claimant without spawning or writing a B record, and A's next fire skips its existing worktree.
#[spec("scheduler/dispatch/041")]
#[test]
fn dispatch_041_two_decks_gate_on_live_github_claim() {
    let preflight = github_preflight();
    skip_unless!(preflight.clone().map(|_| ()));
    let token = preflight.unwrap();
    let scratch = common::harness_tempdir().expect("two-deck fixture directory");
    let github = Github::new(scratch.path(), &token);
    let user = github.read(&["api", "--hostname", "github.com", "user"]);
    let host_id = machine_digest();
    let identity_a = ClaimIdentity {
        user_id: user["id"]
            .as_u64()
            .expect("authenticated GitHub account id"),
        task: SCHEDULE_A,
        host_id: &host_id,
    };
    let identity_b = ClaimIdentity {
        task: SCHEDULE_B,
        ..identity_a
    };
    let issues = github.read(&[
        "issue", "list", "--repo", REPO, "--state", "open", "--label", LABEL, "--json", "number",
    ]);
    assert_eq!(
        issues,
        serde_json::json!([{"number": ISSUE}]),
        "the dedicated label must enumerate only permanent fixture issue #2"
    );
    let before = github.comments();
    let before_a = own_record(&before, &identity_a);
    assert!(
        own_record(&before, &identity_b).is_none(),
        "fixture already has a B record: a prior failed/concurrent run needs investigation"
    );

    let path = stand_in_path(scratch.path());
    let (a, workspace_a, starts_a) = spawn_deck(scratch.path(), SCHEDULE_A, &path, &token);
    let (b, workspace_b, starts_b) = spawn_deck(scratch.path(), SCHEDULE_B, &path, &token);
    assert_ne!(a.home, b.home);
    assert_ne!(a.attach_socket, b.attach_socket);
    assert_ne!(a.hook_socket, b.hook_socket);

    fire_and_wait(&a, SCHEDULE_A, "dispatched issue #2");
    let agents_a = a.agent_records();
    assert!(
        !agents_a.is_empty(),
        "A must dispatch an agent; log:\n{}",
        log(&a)
    );
    common::wait_for_file_lines(&starts_a, agents_a.len(), Duration::from_secs(10))
        .expect("A must execute one local stand-in per registered role");
    let worktree_a =
        dot_agent_deck::issue_dispatch::derive_issue_paths(&workspace_a, SCHEDULE_A, ISSUE)
            .worktree_dir;
    assert!(worktree_a.is_dir(), "A must create its issue worktree");
    let issue = github.read(&["issue", "view", "2", "--repo", REPO, "--json", "labels"]);
    assert!(
        issue["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|label| label["name"] == "agent-deck/claimed"),
        "live issue #2 must carry agent-deck/claimed: {issue}"
    );
    let after_a = github.comments();
    let record_a = own_record(&after_a, &identity_a)
        .expect("A must write a claim for its account, machine and task");
    if let Some(previous) = before_a {
        assert_eq!(
            record_a.comment_id, previous.comment_id,
            "A must re-claim by editing its own existing comment"
        );
        assert_ne!(
            record_a.body, previous.body,
            "A must change its claim body during this run; an old claim cannot prove a write"
        );
    }
    assert!(
        record_a.author_is_trusted(),
        "A's live claim must be authored by a trusted repository account"
    );
    let launches_a = std::fs::read_to_string(&starts_a).expect("A launch ledger");

    fire_and_wait(&b, SCHEDULE_B, "claimed by");
    let b_log = log(&b);
    let claimant = format!(
        "claimed by schedule {:?} on machine {} (GitHub account {})",
        record_a.record.task,
        record_a.record.host,
        record_a.author_login.as_deref().expect("A's GitHub login")
    );
    assert!(
        b_log
            .lines()
            .any(|line| line.contains("skipping already-claimed issue #2")
                && line.contains(&claimant)),
        "B must skip naming A's task, machine and account ({claimant}); log:\n{b_log}"
    );
    assert!(b.agent_records().is_empty(), "B must register no agents");
    assert!(
        !starts_b.exists(),
        "B must execute no local agent, even transiently"
    );
    assert!(
        !dot_agent_deck::issue_dispatch::derive_issue_paths(&workspace_b, SCHEDULE_B, ISSUE)
            .worktree_dir
            .exists(),
        "B must create no issue worktree"
    );
    let after_b = github.comments();
    assert!(
        own_record(&after_b, &identity_b).is_none(),
        "B must write no marked claimant record: {after_b:?}"
    );
    assert_eq!(
        after_b, after_a,
        "B must leave the live issue comments intact"
    );

    fire_and_wait(&a, SCHEDULE_A, "its worktree exists");
    let agent_ids = |daemon: &common::DaemonProc| {
        let mut ids: Vec<_> = daemon
            .agent_records()
            .into_iter()
            .map(|agent| agent.id)
            .collect();
        ids.sort();
        ids
    };
    let mut original_ids: Vec<_> = agents_a.into_iter().map(|agent| agent.id).collect();
    original_ids.sort();
    assert_eq!(
        agent_ids(&a),
        original_ids,
        "A's refire must not add an agent"
    );
    assert_eq!(
        std::fs::read_to_string(starts_a).unwrap(),
        launches_a,
        "A's refire must not execute another stand-in"
    );
    assert!(!log(&a).contains("claim_gating is OFF"));
    assert!(!b_log.contains("claim_gating is OFF"));
    // DaemonProc drops by captured pid/process group; keep claims and labels.
}
