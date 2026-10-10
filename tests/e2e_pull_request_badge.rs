#![cfg(all(unix, feature = "e2e"))]

//! Offline daemon PR resolution and dashboard interaction, using strict PATH
//! stubs and genuine fixture git repositories. No GitHub or agent credentials.

mod common;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{DaemonProc, TuiDeck};
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, KIND_EVENT, KIND_REQ, KIND_RESP,
};
use dot_agent_deck::pull_request::{PullRequestInfo, PullRequestReview, PullRequestState};
use serde_json::{Value, json};
use spec::spec;

const PR_URL: &str = "https://github.com/test-org/test-repo/pull/1234";
const WAIT: Duration = Duration::from_secs(10);

// Reject malformed queries instead of making an incorrect resolver look good.
// Every invocation stays offline, including the deliberate API-failure case.
const GH_STUB: &str = r#"#!/bin/sh
all_args="$*"
[ "$1" = pr ] && [ "$2" = list ] || exit 91
shift 2
head= state= repo= fields=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --head) shift; head="$1" ;;
        --state) shift; state="$1" ;;
        --repo) shift; repo="$1" ;;
        --json) shift; fields="$1" ;;
        *) printf 'unexpected gh flag: %s\n' "$1" >&2; exit 92 ;;
    esac
    shift
done
printf '%s\n' "$all_args" >> "$PR_STUB_DIR/calls"
[ "$repo" = test-org/test-repo ] && [ "$state" = all ] && [ -n "$head" ] || exit 93
for field in number state isDraft reviewDecision url headRefName; do
    case ",$fields," in
        *",$field,"*) ;;
        *) printf 'missing JSON field: %s\n' "$field" >&2; exit 94 ;;
    esac
done
[ ! -f "$PR_STUB_DIR/fail" ] || exit 1
cat "$PR_STUB_DIR/reply.json"
"#;

struct Fixture {
    scratch: tempfile::TempDir,
    repo: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new(branch: &str) -> Self {
        let scratch = common::harness_tempdir().expect("PR fixture scratch");
        let repo = scratch.path().join("repo");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let fixture = Self { scratch, repo, bin };
        fixture.git(&["init", "-b", "main"]);
        fixture.git(&["commit", "--allow-empty", "-m", "fixture"]);
        fixture.git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/test-org/test-repo.git",
        ]);
        // Establish the remote default entirely locally; nothing fetches GitHub.
        fixture.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        fixture.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        if branch != "main" {
            fixture.git(&["checkout", "-b", branch]);
        }
        fixture.executable("gh", GH_STUB);
        fixture.executable(
            "browser",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$PR_STUB_DIR/browser-urls\"\n",
        );
        fixture.set_reply(json!([pr_json(
            1234,
            branch,
            "OPEN",
            false,
            json!("REVIEW_REQUIRED")
        )]));
        fixture
    }

    fn git(&self, args: &[&str]) {
        let output = common::fixture_git(&self.repo, self.scratch.path())
            .args(args)
            .output()
            .expect("fixture git command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn executable(&self, name: &str, content: &str) {
        let path = self.bin.join(name);
        std::fs::write(&path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn set_reply(&self, reply: Value) {
        let pending = self.scratch.path().join("reply.pending");
        std::fs::write(&pending, serde_json::to_vec(&reply).unwrap()).unwrap();
        std::fs::rename(pending, self.scratch.path().join("reply.json")).unwrap();
    }

    fn path_env(&self) -> String {
        format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn daemon(&self) -> DaemonProc {
        common::spawn_daemon_serve_with_env(
            None,
            "0",
            &[
                ("PATH", &self.path_env()),
                ("PR_STUB_DIR", self.scratch.path().to_str().unwrap()),
                ("DOT_AGENT_DECK_PR_REFRESH_SECS", "1"),
                // The stand-in below emits synthetic hook telemetry from the
                // test process; explicitly opt into that existing test seam.
                ("DOT_AGENT_DECK_HOOK_PROVENANCE", "warn"),
            ],
        )
    }

    fn start(&self, daemon: &DaemonProc) -> AgentRecord {
        let response = daemon
            .send_attach_request(&AttachRequest::StartAgent {
                command: Some("cat".into()),
                cwd: Some(self.repo.to_string_lossy().into_owned()),
                rows: 24,
                cols: 80,
                env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "pr-fixture-pane".into())],
                display_name: Some("pr-fixture-agent".into()),
                tab_membership: None,
                agent_type: None,
                seed: None,
                authoring_kind: None,
                client_seeded_kind: None,
                remember_command: false,
            })
            .expect("StartAgent response");
        assert!(response.ok, "start fixture agent: {response:?}");
        let record = daemon
            .wait_for_agent_count(1, WAIT)
            .into_iter()
            .next()
            .expect("registered fixture agent");
        common::write_hook_line(
            &daemon.hook_socket,
            &json!({
                "session_id": "pr-fixture-session",
                "agent_type": "claude_code",
                "event_type": "session_start",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "agent_id": record.id,
                "pane_id": "pr-fixture-pane"
            })
            .to_string(),
        )
        .expect("seed stand-in live session through hook socket");
        daemon
            .wait_for_agent_where(|agent| agent.id == record.id && agent.live.is_some(), WAIT)
            .expect("fixture SessionStart must produce a live snapshot before PR assertions")
    }

    fn tui(&self, daemon: &DaemonProc) -> TuiDeck {
        TuiDeck::builder()
            .with_env(
                "DOT_AGENT_DECK_SOCKET",
                daemon.hook_socket.to_string_lossy().into_owned(),
            )
            .with_env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                daemon.attach_socket.to_string_lossy().into_owned(),
            )
            .with_env(
                "PR_STUB_DIR",
                self.scratch.path().to_string_lossy().into_owned(),
            )
            .with_env(
                "BROWSER",
                self.bin.join("browser").to_string_lossy().into_owned(),
            )
            .launch_with_fixture("minimal")
    }
}

fn pr_json(number: u64, branch: &str, state: &str, draft: bool, review: Value) -> Value {
    json!({"number": number, "headRefName": branch, "state": state,
        "isDraft": draft, "reviewDecision": review,
        "url": format!("https://github.com/test-org/test-repo/pull/{number}")})
}

fn select_dashboard_card(deck: &TuiDeck) {
    deck.wait_until_grid("TUI input mode chip", |grid| {
        grid.lines()
            .any(|line| line.starts_with(" TYPING ") || line.starts_with(" COMMAND "))
    });
    if deck
        .snapshot_grid()
        .lines()
        .any(|line| line.starts_with(" TYPING "))
    {
        deck.send_keys(b"\x04");
    }
    deck.wait_until_grid("dashboard command mode", |grid| {
        grid.lines().any(|line| line.starts_with(" COMMAND "))
    });
    deck.send_keys(b"j");
}

fn expect_pr(daemon: &DaemonProc, expected: &PullRequestInfo) {
    assert!(
        daemon
            .wait_for_agent_where(
                |record| {
                    record
                        .live
                        .as_ref()
                        .and_then(|live| live.pull_request.as_ref())
                        == Some(expected)
                },
                WAIT
            )
            .is_some(),
        "ListAgents must report the resolved pull_request {expected:?}; got {:?}",
        daemon.agent_records()
    );
}

/// Scenario: Start a synthetic agent on a GitHub PR branch and inspect its live
/// daemon listing. Exact branch matching, open-PR preference, lifecycle/review
/// mapping, and the highest-number fallback are reflected in the served PR.
#[spec("session/pr/001")]
#[test]
fn pr_001_daemon_resolves_branch_pull_request() {
    let fixture = Fixture::new("feat/x");
    // A fuzzy match and a newer merged PR must not mask the open exact match.
    fixture.set_reply(json!([
        pr_json(9000, "feat/x-other", "OPEN", false, json!("APPROVED")),
        pr_json(8000, "feat/x", "MERGED", false, json!("APPROVED")),
        pr_json(1234, "feat/x", "OPEN", false, json!("REVIEW_REQUIRED")),
        pr_json(1000, "feat/x", "CLOSED", false, Value::Null)
    ]));
    let daemon = fixture.daemon();
    fixture.start(&daemon);
    expect_pr(
        &daemon,
        &PullRequestInfo {
            number: 1234,
            url: PR_URL.into(),
            state: PullRequestState::Open,
            review: Some(PullRequestReview::ReviewRequired),
        },
    );

    // Fresh daemons exercise startup mapping without imposing polling of a
    // terminal PR, which the contract deliberately stops doing.
    for (state, draft, decision, expected_state, review) in [
        (
            "OPEN",
            true,
            json!("CHANGES_REQUESTED"),
            PullRequestState::Draft,
            Some(PullRequestReview::ChangesRequested),
        ),
        (
            "MERGED",
            false,
            json!("APPROVED"),
            PullRequestState::Merged,
            Some(PullRequestReview::Approved),
        ),
        ("CLOSED", false, Value::Null, PullRequestState::Closed, None),
        ("OPEN", false, json!(""), PullRequestState::Open, None),
    ] {
        let fixture = Fixture::new("feat/x");
        fixture.set_reply(json!([
            pr_json(1000, "feat/x", "CLOSED", false, Value::Null),
            pr_json(1234, "feat/x", state, draft, decision)
        ]));
        let daemon = fixture.daemon();
        fixture.start(&daemon);
        expect_pr(
            &daemon,
            &PullRequestInfo {
                number: 1234,
                url: PR_URL.into(),
                state: expected_state,
                review,
            },
        );
    }
}

/// Scenario: Start agents on the remote default branch and on branches with
/// unavailable PR data, then repeatedly list them after completed git probes.
/// Switching away from a PR branch clears its badge even if the new lookup
/// fails, while a same-branch transient failure preserves the known PR.
#[spec("session/pr/002")]
#[test]
fn pr_002_absent_pull_request_keeps_daemon_healthy() {
    for case in [
        "default",
        "gh-failure",
        "empty",
        "detached",
        "non-github",
        "non-git",
    ] {
        let fixture = Fixture::new(if case == "default" { "main" } else { "feat/x" });
        if case == "default" {
            // Record completion, not just launch. Seeing a second default-
            // branch probe proves the monitor got through its first pass.
            fixture.executable("git", "#!/bin/sh\n/usr/bin/git \"$@\"\nresult=$?\nprintf '%s\\n' \"$*\" >> \"$PR_STUB_DIR/git-probes\"\nexit \"$result\"\n");
        }
        match case {
            "gh-failure" => std::fs::write(fixture.scratch.path().join("fail"), "fail").unwrap(),
            "empty" => fixture.set_reply(json!([])),
            "detached" => fixture.git(&["checkout", "--detach"]),
            "non-github" => fixture.git(&[
                "remote",
                "set-url",
                "origin",
                "https://gitlab.com/test-org/test-repo.git",
            ]),
            "non-git" => std::fs::remove_dir_all(fixture.repo.join(".git")).unwrap(),
            _ => {}
        }
        let mut daemon = fixture.daemon();
        fixture.start(&daemon);
        assert!(
            !common::wait_until(Duration::from_secs(2), || {
                let records = daemon.agent_records();
                records.len() != 1
                    || records[0]
                        .live
                        .as_ref()
                        .and_then(|live| live.pull_request.as_ref())
                        .is_some()
            }),
            "agent must stay served without a PR for {case}: {:?}",
            daemon.agent_records()
        );
        assert!(
            daemon.is_alive_public(),
            "daemon remains healthy for {case}"
        );
        if case == "default" {
            assert!(
                common::wait_until(WAIT, || {
                    std::fs::read_to_string(fixture.scratch.path().join("git-probes"))
                        .unwrap_or_default()
                        .lines()
                        .filter(|line| {
                            *line == "symbolic-ref --quiet --short refs/remotes/origin/HEAD"
                        })
                        .count()
                        >= 2
                }),
                "two completed default-branch probes must precede the no-gh assertion"
            );
            assert!(
                !fixture.scratch.path().join("calls").exists(),
                "default branch must not query gh"
            );
        }
    }

    for case in [
        "same-key-failure",
        "default",
        "detached",
        "other-branch-failure",
    ] {
        let fixture = Fixture::new("feat/x");
        let daemon = fixture.daemon();
        fixture.start(&daemon);
        let open = PullRequestInfo {
            number: 1234,
            url: PR_URL.into(),
            state: PullRequestState::Open,
            review: Some(PullRequestReview::ReviewRequired),
        };
        expect_pr(&daemon, &open);
        let calls_before = std::fs::read_to_string(fixture.scratch.path().join("calls"))
            .unwrap()
            .lines()
            .count();
        std::fs::write(fixture.scratch.path().join("fail"), "fail").unwrap();
        match case {
            "default" => fixture.git(&["checkout", "main"]),
            "detached" => fixture.git(&["checkout", "--detach"]),
            "other-branch-failure" => fixture.git(&["checkout", "-b", "feat/y"]),
            _ => {}
        }
        if matches!(case, "same-key-failure" | "other-branch-failure") {
            assert!(
                common::wait_until(WAIT, || {
                    let calls = std::fs::read_to_string(fixture.scratch.path().join("calls"))
                        .unwrap_or_default();
                    if case == "other-branch-failure" {
                        calls.lines().any(|line| line.contains("--head feat/y "))
                    } else {
                        calls.lines().count() > calls_before
                    }
                }),
                "resolver must attempt the failing lookup for {case}"
            );
        }
        if case == "same-key-failure" {
            assert!(
                !common::wait_until(Duration::from_secs(2), || {
                    let records = daemon.agent_records();
                    records.len() != 1
                        || records[0]
                            .live
                            .as_ref()
                            .and_then(|live| live.pull_request.as_ref())
                            != Some(&open)
                }),
                "same-key transient failure must preserve the known PR"
            );
        } else {
            assert!(
                daemon
                    .wait_for_agent_where(
                        |record| {
                            record
                                .live
                                .as_ref()
                                .is_some_and(|live| live.pull_request.is_none())
                        },
                        WAIT
                    )
                    .is_some(),
                "switching away from feat/x must clear its old PR for {case}; got {:?}",
                daemon.agent_records()
            );
        }
    }
}

// Join on unwinding too: a failing assertion must stop its synthetic producer.
struct ToolTraffic {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ToolTraffic {
    fn start(daemon: &DaemonProc, agent: &AgentRecord) -> Self {
        let (stop, stopped) = std::sync::mpsc::channel();
        let socket = daemon.hook_socket.clone();
        let id = agent.id.clone();
        let thread = std::thread::spawn(move || {
            loop {
                common::write_hook_line(
                    &socket,
                    &json!({
                        "session_id": "pr-fixture-session",
                        "agent_type": "claude_code",
                        "event_type": "tool_start",
                        "tool_name": "Read",
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                        "agent_id": id,
                        "pane_id": "pr-fixture-pane"
                    })
                    .to_string(),
                )
                .expect("synthetic non-trigger tool event");
                // This timeout generates arrivals at a deliberate cadence;
                // all assertions wait on observable state, not elapsed time.
                if !matches!(
                    stopped.recv_timeout(Duration::from_millis(20)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for ToolTraffic {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().expect("tool traffic producer stopped");
        }
    }
}

// A protocol client holds ONE subscription throughout the transition. Read
// arbitrary broadcast JSON so this test does not dictate the event used by the
// implementation; nested metadata JSON strings are also valid payloads.
fn subscribe(socket: &Path) -> UnixStream {
    let mut stream = UnixStream::connect(socket).expect("connect event client");
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let request = serde_json::to_vec(&AttachRequest::SubscribeEvents).unwrap();
    stream.write_all(&[KIND_REQ]).unwrap();
    stream
        .write_all(&(request.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&request).unwrap();
    let (kind, body) = read_frame(&mut stream).expect("subscription ack");
    assert_eq!(kind, KIND_RESP);
    assert!(serde_json::from_slice::<AttachResponse>(&body).unwrap().ok);
    stream
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    stream.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(len <= 16 * 1024 * 1024, "bounded broadcast frame");
    let mut body = vec![0; len];
    stream.read_exact(&mut body)?;
    Ok((header[0], body))
}

fn contains_pr(value: &Value, expected: &PullRequestInfo) -> bool {
    if serde_json::from_value::<PullRequestInfo>(value.clone()).is_ok_and(|pr| &pr == expected) {
        return true;
    }
    match value {
        Value::Object(fields) => fields.values().any(|v| contains_pr(v, expected)),
        Value::Array(values) => values.iter().any(|v| contains_pr(v, expected)),
        Value::String(text) => {
            serde_json::from_str::<Value>(text).is_ok_and(|v| contains_pr(&v, expected))
        }
        _ => false,
    }
}

/// Scenario: Keep a headless client subscribed while an open PR becomes merged
/// and approved behind the gh stub, with continuous non-trigger tool events.
/// Both ListAgents and that same attached connection receive the new PR within
/// a bounded number of shortened poll intervals despite the broadcast traffic.
#[spec("session/pr/003")]
#[test]
fn pr_003_refresh_reaches_attached_client_without_reconnect() {
    let fixture = Fixture::new("feat/x");
    let daemon = fixture.daemon();
    let mut client = subscribe(&daemon.attach_socket);
    let agent = fixture.start(&daemon);
    expect_pr(
        &daemon,
        &PullRequestInfo {
            number: 1234,
            url: PR_URL.into(),
            state: PullRequestState::Open,
            review: Some(PullRequestReview::ReviewRequired),
        },
    );
    let _traffic = ToolTraffic::start(&daemon, &agent);
    assert!(
        daemon
            .wait_for_agent_where(
                |record| record.live.as_ref().is_some_and(|live| {
                    live.active_tool
                        .as_ref()
                        .is_some_and(|tool| tool.name == "Read")
                }),
                WAIT,
            )
            .is_some(),
        "non-trigger tool traffic must reach the live session before changing the gh reply"
    );
    fixture.set_reply(json!([pr_json(
        1234,
        "feat/x",
        "MERGED",
        false,
        json!("APPROVED")
    )]));
    let expected = PullRequestInfo {
        number: 1234,
        url: PR_URL.into(),
        state: PullRequestState::Merged,
        review: Some(PullRequestReview::Approved),
    };
    assert!(
        daemon
            .wait_for_agent_where(
                |record| record
                    .live
                    .as_ref()
                    .and_then(|live| live.pull_request.as_ref())
                    == Some(&expected),
                WAIT,
            )
            .is_some(),
        "continuous tool traffic must not starve periodic PR refresh; gh calls: {}; agents: {:?}",
        std::fs::read_to_string(fixture.scratch.path().join("calls")).unwrap_or_default(),
        daemon.agent_records()
    );
    let deadline = Instant::now() + WAIT;
    let mut observed = Vec::new();
    let mut received = false;
    while Instant::now() < deadline {
        client
            .set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))
            .unwrap();
        let Ok((kind, body)) = read_frame(&mut client) else {
            break;
        };
        if kind == KIND_EVENT {
            let value: Value = serde_json::from_slice(&body).expect("broadcast JSON");
            received = contains_pr(&value, &expected);
            observed.push(value);
            if received {
                break;
            }
        }
    }
    assert!(
        received,
        "already-attached client must receive merged/approved PR without reconnecting; observed {observed:?}"
    );
}

/// Scenario: Attach the real TUI to a synthetic agent on a PR branch. Its
/// dashboard card shows #1234 with open/review-required glyphs, and pressing o invokes the configured browser
/// with the exact GitHub PR URL; the same key on a default-branch card opens
/// nothing and leaves the dashboard usable.
#[spec("session/pr/004")]
#[test]
fn pr_004_dashboard_badge_and_browser_shortcut() {
    // Exercise the no-PR shortcut first so RED also validates input setup.
    {
        let fixture = Fixture::new("main");
        let daemon = fixture.daemon();
        fixture.start(&daemon);
        let deck = fixture.tui(&daemon);
        deck.wait_for_string("pr-fixture-agent");
        select_dashboard_card(&deck);
        deck.send_keys(b"o?");
        deck.wait_for_string("Global (works from any pane)");
        assert!(
            !fixture.scratch.path().join("browser-urls").exists(),
            "o on a card without a PR must not spawn an opener"
        );
        let records = daemon.agent_records();
        assert_eq!(records.len(), 1, "o must leave the no-PR agent served");
        assert!(
            records[0]
                .live
                .as_ref()
                .and_then(|live| live.pull_request.as_ref())
                .is_none()
        );
    }
    let fixture = Fixture::new("feat/x");
    let daemon = fixture.daemon();
    fixture.start(&daemon);
    let deck = fixture.tui(&daemon);
    deck.wait_for_string("pr-fixture-agent");
    assert!(
        deck.wait_for_grid_string_within("#1234 ⊙ ◐", WAIT),
        "dashboard card must show #1234 with open/review-required glyphs:\n{}",
        deck.snapshot_grid()
    );
    select_dashboard_card(&deck);
    deck.send_keys(b"o");
    common::wait_for_file_trimmed_eq(&fixture.scratch.path().join("browser-urls"), PR_URL, WAIT)
        .expect("o must open the selected PR URL through BROWSER");
}
