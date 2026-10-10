#![cfg(all(unix, feature = "e2e", feature = "e2e-live"))]

//! Interactive Haiku PR-badge journey; GitHub lookup stays offline.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::AgentType;
use spec::spec;

const SENTINEL: &str = "PR_BADGE_LIVE_85BC9576_SENTINEL.txt";
const PANE_ID: &str = "pr-badge-live-pane";
const WAIT: Duration = Duration::from_secs(180);

// Reject a query with the wrong repository, branch, state selection or fields.
// This command never invokes the host gh or a remote GitHub endpoint.
const GH_STUB: &str = r#"#!/bin/sh
[ "$1" = pr ] && [ "$2" = list ] || exit 91
shift 2
head= state= repo= fields=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --head) shift; head="$1" ;;
        --state) shift; state="$1" ;;
        --repo) shift; repo="$1" ;;
        --json) shift; fields="$1" ;;
        *) exit 92 ;;
    esac
    shift
done
[ "$head" = feat/pr-badge ] && [ "$repo" = test-org/test-repo ] && [ "$state" = all ] || exit 93
for field in number state isDraft reviewDecision url headRefName; do
    case ",$fields," in
        *",$field,"*) ;;
        *) exit 94 ;;
    esac
done
printf '%s\n' '[{"number":1234,"headRefName":"feat/pr-badge","state":"OPEN","isDraft":false,"reviewDecision":"REVIEW_REQUIRED","url":"https://github.com/test-org/test-repo/pull/1234"}]'
"#;

/// Scenario: Launch an interactive Haiku agent in a fixture PR branch and ask
/// it to list the files without naming the sentinel in the prompt. The attached
/// TUI must show the discovered filename alongside the open/review-required PR badge.
#[spec("session/pr/005")]
#[test]
fn pr_005_real_haiku_work_and_pull_request_badge() {
    skip_unless!(common::check_claude_available());

    let scratch = common::harness_tempdir().expect("live PR fixture scratch");
    let repo = scratch.path().join("repo");
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["commit", "--allow-empty", "-m", "fixture"],
        vec![
            "remote",
            "add",
            "origin",
            "https://github.com/test-org/test-repo.git",
        ],
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        vec![
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
        vec!["checkout", "-b", "feat/pr-badge"],
    ] {
        let output = common::fixture_git(&repo, scratch.path())
            .args(&args)
            .output()
            .expect("fixture git command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::write(repo.join(SENTINEL), "real Haiku PR badge fixture\n").unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, GH_STUB).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let repo = repo.canonicalize().expect("canonical fixture repo");
    let deck = TuiDeck::builder()
        .with_pty_size(160, 45)
        .with_imported_claude_credentials()
        .with_claude_project_trust(repo.to_string_lossy())
        .with_env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .with_env("DOT_AGENT_DECK_PR_REFRESH_SECS", "1")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    let events = deck.subscribe_events();
    let response = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StartAgent {
            command: Some("claude --ax-screen-reader --model claude-haiku-4-5-20251001 --allowedTools Bash Read".into()),
            cwd: Some(repo.to_string_lossy().into_owned()),
            rows: 36,
            cols: 156,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), PANE_ID.into())],
            display_name: Some("PR badge Haiku".into()),
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            seed: None,
            authoring_kind: None,
            client_seeded_kind: None,
            remember_command: false,
        },
    ).expect("start interactive Haiku");
    assert!(response.ok, "StartAgent: {response:?}");
    let agent_id = response.id.expect("started Haiku agent id");
    events.wait_for_session_start_on_pane(PANE_ID, &agent_id, WAIT);
    deck.wait_for_absence("No active agents");
    deck.send_keys(b"1");
    assert!(
        deck.wait_for_grid_string_within("#1234 ⊙ ◐", WAIT),
        "real-agent card must show PR number, state and review:\n{}",
        deck.snapshot_grid()
    );
    assert!(
        common::wait_until_panes_settled(
            deck.attach_socket_path(),
            std::slice::from_ref(&agent_id),
            Duration::from_millis(1500),
            Duration::from_secs(8),
            WAIT,
        ),
        "interactive Claude startup must settle before typing:\n{}",
        deck.snapshot_grid()
    );
    if deck
        .snapshot_grid()
        .lines()
        .any(|line| line.starts_with(" COMMAND "))
    {
        deck.send_keys(b"\x04");
    }
    deck.wait_until_grid("interactive Haiku typing mode", |grid| {
        grid.lines().any(|line| line.starts_with(" TYPING "))
    });
    deck.submit_claude_prompt(
        &events,
        &agent_id,
        "PR_BADGE_LIST_FILES. Use Bash to run ls -1 in the current directory, then print every filename verbatim, one per line, with no commentary.",
        "PR_BADGE_LIST_FILES",
    );
    assert!(
        deck.wait_for_grid_predicate_within(WAIT, |grid| {
            grid.contains(SENTINEL) && grid.contains("#1234 ⊙ ◐")
        }),
        "the attached TUI must visibly show the discovered sentinel and PR badge together:\n{}",
        deck.snapshot_grid()
    );
}
