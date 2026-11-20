#![cfg(all(feature = "e2e", unix))]

//! Issue #2418 (PRD #2374): the deck's own check of the pull requests and
//! issues an agent names on its card, end to end through the real binary and
//! a real daemon, with a stand-in `gh` on the daemon's `PATH` (no GitHub, no
//! credential, lane 1).
//!
//! An agent publishes its card with `dot-agent-deck card publish` from its
//! pane's identity, and the user reads it with `card list` from a shell
//! outside any pane, as the desktop app's cards do. The assertions are on the
//! JSON `card list` prints (`checked_refs`: the agent's claim beside the
//! deck's result) and on the `gh` calls the stand-in records, never on the
//! daemon's internals.
//!
//! The stand-in `gh` answers only `gh pr view <n> --repo <owner/name> --json
//! <fields>` and `gh issue view <n> --repo <owner/name> --json <fields>`, the
//! lookups the daemon runs, from files the test writes; anything else exits
//! non-zero, so a wrong argv reads as a failed check rather than a pass.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use inbox::{Endpoint, Pane};
use serde_json::{Value, json};
use spec::spec;

const WAIT: Duration = Duration::from_secs(20);

/// The repository the fixture folder's `origin` names: a bare `#N` on a card
/// whose agent works there resolves against it.
const ORIGIN_REPO: &str = "test-org/test-repo";

/// The daemon's `gh`. It records every call as it starts, waits while the
/// reference's `.hold` file exists, then reads the reference's `.current`
/// pointer ONCE and answers from the bundle directory it names: that
/// bundle's `stderr` and `json`, and its `exit` code.
///
/// The hold has no clock of its own: it ends when the test removes the file,
/// when the daemon's own `gh` timeout kills this process, or when the
/// fixture's directory is deleted at teardown. Bundles are written in full
/// before the pointer names them and never change afterwards
/// ([`RefDeck::answer`]), so a call reads one whole answer, never a mix of
/// two.
const GH_STUB: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$GH_STUB_DIR/calls"
case "$1 $2" in
    "pr view"|"issue view") ;;
    *) printf 'unexpected gh call: %s\n' "$*" >&2; exit 91 ;;
esac
[ "$4" = --repo ] && [ "$6" = --json ] && [ "$#" -eq 7 ] || { printf 'unexpected gh argv: %s\n' "$*" >&2; exit 92; }
answer="$GH_STUB_DIR/answers/$1-$(printf '%s' "$5" | tr '/' '_')-$3"
while [ -f "$answer.hold" ]; do
    sleep 0.1
done
bundle="$(cat "$answer.current" 2>/dev/null)"
[ -n "$bundle" ] && [ -f "$GH_STUB_DIR/answers/$bundle/exit" ] || { printf 'no answer scripted for: %s\n' "$*" >&2; exit 93; }
dir="$GH_STUB_DIR/answers/$bundle"
[ ! -f "$dir/stderr" ] || cat "$dir/stderr" >&2
[ ! -f "$dir/json" ] || cat "$dir/json"
exit "$(cat "$dir/exit")"
"#;

/// A `gh` a CLIENT finds on its own `PATH`: it records that it ran and
/// answers nothing, so a client that looked a reference up itself would leave
/// a call behind and could not read a state from it.
const CLIENT_GH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$CLIENT_GH_CALLS"
exit 1
"#;

/// A daemon with a stand-in `gh` (or, with `gh_missing`, no `gh` at all) and
/// the folders its agents work in.
struct RefDeck {
    daemon: common::DaemonProc,
    scratch: tempfile::TempDir,
    /// A git repository whose `origin` is `github.com/test-org/test-repo`.
    repo: PathBuf,
}

fn executable(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The first `name` on the test process's `PATH`.
fn on_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("{name} must be on the test's PATH"))
}

impl RefDeck {
    /// `gh_missing`: the daemon's `PATH` holds only `git` and `cat`, so the
    /// daemon finds no `gh` to run.
    fn new(gh_missing: bool) -> Self {
        let scratch = common::harness_tempdir().expect("reference fixture scratch");
        let repo = scratch.path().join("repo");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(scratch.path().join("answers")).unwrap();
        let git = |args: &[&str]| {
            let out = common::fixture_git(&repo, scratch.path())
                .args(args)
                .output()
                .expect("fixture git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["commit", "--allow-empty", "-m", "fixture"]);
        git(&[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{ORIGIN_REPO}.git"),
        ]);
        let path = if gh_missing {
            for tool in ["git", "cat"] {
                std::os::unix::fs::symlink(on_path(tool), bin.join(tool)).unwrap();
            }
            bin.to_string_lossy().into_owned()
        } else {
            executable(&bin.join("gh"), GH_STUB);
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            )
        };
        let stub_dir = scratch.path().to_string_lossy().into_owned();
        let daemon = inbox::daemon_unbound(&[
            ("PATH", path.as_str()),
            ("GH_STUB_DIR", stub_dir.as_str()),
            // An open reference is read again every second, so a later
            // failure is seen within the test's wait.
            ("DOT_AGENT_DECK_PR_REFRESH_SECS", "1"),
        ]);
        Self {
            daemon,
            scratch,
            repo,
        }
    }

    fn answer_base(&self, kind: &str, repo: &str, number: u64) -> PathBuf {
        self.scratch
            .path()
            .join("answers")
            .join(format!("{kind}-{}-{number}", repo.replace('/', "_")))
    }

    /// Script `gh <kind> view <number> --repo <repo>`'s answer, replacing any
    /// earlier one for every call that starts reading it afterwards. The
    /// answer is a new bundle directory, written in full and never touched
    /// again; the reference's `.current` pointer is then swapped to name it
    /// with one `rename`, so a call that already read the pointer finishes
    /// with the old bundle and a later one reads only the new.
    fn answer(&self, kind: &str, repo: &str, number: u64, exit: i32, stdout: &str, stderr: &str) {
        let base = self.answer_base(kind, repo, number);
        let name = base.file_name().unwrap().to_string_lossy().into_owned();
        let bundle = format!("{name}.{}", unique());
        let dir = base.with_file_name(&bundle);
        std::fs::create_dir(&dir).unwrap();
        for (file, content) in [("json", stdout), ("stderr", stderr)] {
            if !content.is_empty() {
                std::fs::write(dir.join(file), content).unwrap();
            }
        }
        std::fs::write(dir.join("exit"), exit.to_string()).unwrap();
        let pointer = base.with_extension("current-pending");
        std::fs::write(&pointer, &bundle).unwrap();
        std::fs::rename(pointer, base.with_extension("current")).unwrap();
    }

    /// How many lookups of `gh <kind> view <number> --repo <repo>` have
    /// STARTED. The daemon never runs two lookups of one reference at once
    /// and records each answer before it starts the next, so a count of
    /// `n + 1` means the `n`th answer is already in what `card list` shows.
    fn lookups_of(&self, kind: &str, repo: &str, number: u64) -> usize {
        let prefix = format!("{kind} view {number} --repo {repo} ");
        self.lookups()
            .iter()
            .filter(|c| c.starts_with(&prefix))
            .count()
    }

    fn hold(&self, kind: &str, repo: &str, number: u64, held: bool) {
        let path = self.answer_base(kind, repo, number).with_extension("hold");
        if held {
            std::fs::write(path, "").unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }

    /// Every reference lookup the daemon ran with `gh`, one argv per line.
    /// The branch badge's own `gh pr list`, should the daemon run one, is
    /// not a reference lookup and is left out.
    fn lookups(&self) -> Vec<String> {
        std::fs::read_to_string(self.scratch.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|c| c.starts_with("pr view ") || c.starts_with("issue view "))
            .map(str::to_string)
            .collect()
    }

    /// A pane in `cwd` whose process keeps its capability where the test can
    /// read it, so the test can publish as its agent.
    fn agent(&self, name: &str, cwd: &Path) -> Pane {
        let reply = inbox::attach(
            &self.daemon,
            json!({"op":"start-agent",
                "command":common::capability_export_command("exec cat"),
                "cwd":cwd, "rows":24, "cols":80,
                "env":[["DOT_AGENT_DECK_PANE_ID",name]], "display_name":name}),
        );
        assert_eq!(reply["ok"], true, "start {name}: {reply}");
        let agent = reply["id"].as_str().unwrap().to_string();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(cwd, &agent))
            .expect("the pane's capability");
        Pane {
            id: name.into(),
            agent,
            token,
            pid: 0,
            capture: cwd.join(format!(".{name}.bytes")),
            attach_socket: self.daemon.attach_socket().to_path_buf(),
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr)
    )
}

/// The real CLI with the deck's sockets, `path` as its `PATH`, and exactly
/// the pane identity in `identity` (none: the user's shell).
fn cli(
    endpoint: &impl Endpoint,
    path: &str,
    extra_env: &[(&str, &str)],
    identity: &[(&str, &str)],
    args: &[&str],
) -> Output {
    let mut command = std::process::Command::new(common::contained_deck_binary());
    command
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", endpoint.home())
        .env("DOT_AGENT_DECK_SOCKET", endpoint.hook_socket())
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", endpoint.attach_socket())
        .current_dir(endpoint.cwd());
    for (key, value) in extra_env.iter().chain(identity) {
        command.env(key, value);
    }
    command.output().expect("run the real deck CLI")
}

/// `pane`'s agent publishes `report` with `card publish --file`.
fn publish(endpoint: &impl Endpoint, pane: &Pane, report: &Value) {
    let file = endpoint
        .home()
        .join(format!("card-{}-{}.json", pane.agent, unique()));
    std::fs::write(&file, report.to_string()).unwrap();
    let identity = [
        ("DOT_AGENT_DECK_PANE_ID", pane.id.as_str()),
        ("DOT_AGENT_DECK_AGENT_ID", pane.agent.as_str()),
        ("DOT_AGENT_DECK_PANE_CAPABILITY", pane.token.as_str()),
    ];
    let out = cli(
        endpoint,
        "/usr/bin:/bin",
        &[],
        &identity,
        &["card", "publish", "--file", &file.to_string_lossy()],
    );
    assert_eq!(out.status.code(), Some(0), "publish: {}", describe(&out));
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// `card list` from the user's shell, parsed.
fn list(endpoint: &impl Endpoint) -> Value {
    list_with(endpoint, "/usr/bin:/bin", &[])
}

fn list_with(endpoint: &impl Endpoint, path: &str, extra_env: &[(&str, &str)]) -> Value {
    let out = cli(endpoint, path, extra_env, &[], &["card", "list"]);
    assert_eq!(out.status.code(), Some(0), "card list: {}", describe(&out));
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("card list prints JSON ({e}): {}", text(&out.stdout)))
}

fn key(pane: &Pane) -> String {
    format!("{}/{}", pane.id, pane.agent)
}

/// `pane`'s checked reference from `field`, in `doc`.
fn checked_in(doc: &Value, pane: &Pane, field: &str) -> Option<Value> {
    doc["entries"]
        .as_array()?
        .iter()
        .find(|e| e["key"] == key(pane).as_str())?["checked_refs"]
        .as_array()?
        .iter()
        .find(|r| r["field"] == field)
        .cloned()
}

/// Poll `card list` until `pane`'s reference from `field` satisfies `pred`;
/// returns it.
fn wait_ref(
    endpoint: &impl Endpoint,
    pane: &Pane,
    field: &str,
    what: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let last = RefCell::new(Value::Null);
    let ok = common::wait_until(common::load_scaled(WAIT), || {
        let doc = list(endpoint);
        let found = checked_in(&doc, pane, field);
        *last.borrow_mut() = doc;
        found.is_some_and(|r| pred(&r))
    });
    assert!(
        ok,
        "{}'s {field} reference never became {what}: {:#}",
        pane.id,
        last.borrow()
    );
    checked_in(&last.borrow(), pane, field).unwrap()
}

fn status_is(status: &'static str) -> impl Fn(&Value) -> bool {
    move |r| r["result"]["status"] == status
}

fn rfc3339(at: &Value) -> chrono::DateTime<chrono::FixedOffset> {
    at.as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .unwrap_or_else(|| panic!("checked_at is an RFC 3339 time: {at}"))
}

/// The pending handshake. For each candidate number in turn: hold the
/// stand-in's answer for `#<number>`, have `pane`'s agent publish a card
/// naming it, and poll until the card reads pending, resolved against the
/// folder's origin, with that lookup started — then release the hold and
/// return the number. The hold is released as soon as the poll ends, before
/// the test waits on anything else, so nothing but the daemon's own `gh`
/// timeout bounds it.
///
/// Should that timeout end the held lookup before the test sees it pending (a
/// starved runner), the card no longer reads pending; the candidate is
/// released and the card names the next one instead, a fresh reference with
/// no answer yet.
fn observe_pending_then_release(deck: &RefDeck, pane: &Pane, candidates: &[u64]) -> u64 {
    let mut missed = Vec::new();
    for &number in candidates {
        deck.hold("pr", ORIGIN_REPO, number, true);
        deck.answer(
            "pr",
            ORIGIN_REPO,
            number,
            0,
            &pr_json(
                ORIGIN_REPO,
                number,
                "OPEN",
                "REVIEW_REQUIRED",
                json!([]),
                "REFS-002 open one",
            ),
            "",
        );
        let claim = format!("#{number}");
        publish(
            &deck.daemon,
            pane,
            &json!({"status": "working", "fields": [
                {"name": "pull-request", "value": claim}
            ]}),
        );
        let last = RefCell::new(None);
        let ended = common::wait_until(common::load_scaled(WAIT), || {
            // The lookup count is read before the card, so a started lookup
            // is one that had started when the card was read.
            let started = deck.lookups_of("pr", ORIGIN_REPO, number) > 0;
            let found = checked_in(&list(&deck.daemon), pane, "pull-request")
                .filter(|r| r["claim"] == claim.as_str());
            let ended = found.as_ref().is_some_and(|r| {
                r["result"] != json!({"status": "pending"}) || (started && r["repo"] == ORIGIN_REPO)
            });
            *last.borrow_mut() = found;
            ended
        });
        deck.hold("pr", ORIGIN_REPO, number, false);
        let found = last.into_inner();
        assert!(
            ended,
            "{claim}'s lookup never started while it was held: {found:#?}"
        );
        let found = found.unwrap();
        if found["result"] == json!({"status": "pending"}) {
            assert_eq!(
                found["url"],
                format!("https://github.com/{ORIGIN_REPO}/pull/{number}"),
                "{found:#}"
            );
            return number;
        }
        missed.push(found);
    }
    panic!("no held lookup was seen pending before the daemon's gh timeout ended it: {missed:#?}");
}

/// Wait until at least `count` lookups of `ORIGIN_REPO`'s pull request
/// `number` have started, then return `pane`'s pull-request reference as
/// `card list` shows it: every answer before the last started lookup is in
/// it ([`RefDeck::lookups_of`]).
fn after_lookups(deck: &RefDeck, pane: &Pane, number: u64, count: usize) -> Value {
    let reached = common::wait_until(common::load_scaled(WAIT), || {
        deck.lookups_of("pr", ORIGIN_REPO, number) >= count
    });
    assert!(
        reached,
        "#{number} was looked up {} times, waiting for {count}: {:?}",
        deck.lookups_of("pr", ORIGIN_REPO, number),
        deck.lookups()
    );
    checked_in(&list(&deck.daemon), pane, "pull-request").unwrap()
}

/// What `gh pr view --json` prints for a pull request.
fn pr_json(
    repo: &str,
    number: u64,
    state: &str,
    review: &str,
    checks: Value,
    title: &str,
) -> String {
    json!({
        "number": number,
        "state": state,
        "isDraft": false,
        "reviewDecision": review,
        "url": format!("https://github.com/{repo}/pull/{number}"),
        "headRefName": "feature",
        "statusCheckRollup": checks,
        "title": title,
    })
    .to_string()
}

/// What `gh issue view --json` prints for an issue.
fn issue_json(repo: &str, number: u64, state: &str, title: &str) -> String {
    json!({
        "number": number,
        "state": state,
        "title": title,
        "url": format!("https://github.com/{repo}/issues/{number}"),
    })
    .to_string()
}

const PR_ARGV_FIELDS: &str =
    "number,state,isDraft,reviewDecision,url,headRefName,statusCheckRollup,title";

/// Scenario: An agent working in a clone of `test-org/test-repo` publishes a
/// card naming pull request `#41` and issue `other-org/other-repo#7`, which
/// the daemon's stand-in `gh` says are a merged pull request whose CI failed
/// and a closed issue. `card list` shows each claim exactly as the agent wrote
/// it beside the deck's check: merged with failing CI and approved, closed,
/// both with canonical github.com links, and the daemon asked `gh` once for
/// each and did not ask again in the seconds after, once they were settled.
#[spec("agent-report/refs/001")]
#[test]
fn refs_001_reported_references_show_the_decks_own_check() {
    let deck = RefDeck::new(false);
    deck.answer(
        "pr",
        ORIGIN_REPO,
        41,
        0,
        &pr_json(
            ORIGIN_REPO,
            41,
            "MERGED",
            "APPROVED",
            json!([
                {"status": "COMPLETED", "conclusion": "FAILURE", "name": "build"},
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"}
            ]),
            "REFS-001 add the widget",
        ),
        "",
    );
    deck.answer(
        "issue",
        "other-org/other-repo",
        7,
        0,
        &issue_json(
            "other-org/other-repo",
            7,
            "CLOSED",
            "REFS-001 widget is missing",
        ),
        "",
    );
    let agent = deck.agent("refs-agent", &deck.repo);
    publish(
        &deck.daemon,
        &agent,
        &json!({
            "status": "working",
            "summary": "REFS-001 opened the pull request",
            "fields": [
                {"name": "pull-request", "value": "#41"},
                {"name": "issue", "value": "other-org/other-repo#7"}
            ]
        }),
    );

    let pr = wait_ref(
        &deck.daemon,
        &agent,
        "pull-request",
        "checked",
        status_is("checked"),
    );
    assert_eq!(pr["kind"], "pull_request", "{pr:#}");
    assert_eq!(pr["claim"], "#41", "the agent's claim is kept as written");
    assert_eq!(
        pr["repo"], ORIGIN_REPO,
        "a bare #N resolves in the agent's repo"
    );
    assert_eq!(pr["number"], 41);
    assert_eq!(pr["url"], "https://github.com/test-org/test-repo/pull/41");
    let result = &pr["result"];
    assert_eq!(result["state"], "merged", "{pr:#}");
    assert_eq!(
        result["ci"],
        json!({"state": "failing", "passing": 1, "failing": 1, "pending": 0, "skipped": 0}),
        "{pr:#}"
    );
    assert_eq!(result["review"], "approved", "{pr:#}");
    assert_eq!(result["title"], "REFS-001 add the widget");
    assert_eq!(result["stale"], false);
    assert!(
        result["checked_at"]
            .as_str()
            .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok()),
        "checked_at is an RFC 3339 time: {pr:#}"
    );

    let issue = wait_ref(
        &deck.daemon,
        &agent,
        "issue",
        "checked",
        status_is("checked"),
    );
    assert_eq!(issue["kind"], "issue", "{issue:#}");
    assert_eq!(issue["claim"], "other-org/other-repo#7");
    assert_eq!(issue["repo"], "other-org/other-repo");
    assert_eq!(issue["number"], 7);
    assert_eq!(
        issue["url"],
        "https://github.com/other-org/other-repo/issues/7"
    );
    assert_eq!(issue["result"]["state"], "closed", "{issue:#}");
    assert_eq!(issue["result"]["title"], "REFS-001 widget is missing");
    assert!(issue["result"].get("ci").is_none(), "{issue:#}");
    assert!(issue["result"].get("review").is_none(), "{issue:#}");

    // The agent's own field values are unchanged beside the deck's check.
    let doc = list(&deck.daemon);
    let entry = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == key(&agent).as_str())
        .unwrap();
    let values: Vec<_> = entry["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].clone(), f["value"].clone(), f["source"].clone()))
        .collect();
    assert_eq!(
        values,
        [
            (json!("pull-request"), json!("#41"), json!("agent")),
            (
                json!("issue"),
                json!("other-org/other-repo#7"),
                json!("agent")
            ),
        ]
    );

    // A merged pull request with finished checks and a closed issue are
    // settled: the daemon read each once and, with a one-second refresh,
    // does not read them again.
    // Sorted, as the calls are below: the two lookups run side by side.
    let expected = [
        "issue view 7 --repo other-org/other-repo --json number,state,title,url".to_string(),
        format!("pr view 41 --repo {ORIGIN_REPO} --json {PR_ARGV_FIELDS}"),
    ];
    let mut calls = deck.lookups();
    calls.sort();
    assert_eq!(calls, expected, "one lookup each, with the documented argv");
    // Three one-second refresh intervals with no further call: polled
    // throughout, so a repeat lookup fails the test as soon as it is made.
    let sorted_lookups = || {
        let mut calls = deck.lookups();
        calls.sort();
        calls
    };
    let looked_again = common::wait_until(common::load_scaled(Duration::from_secs(3)), || {
        sorted_lookups() != expected
    });
    assert!(
        !looked_again,
        "a settled reference is not looked up again: {:?}",
        sorted_lookups()
    );
}

/// Scenario: Agents publish cards naming references the deck cannot check:
/// one whose lookup the stand-in `gh` holds (it reads pending until the
/// answer comes, then open), one `gh` says it cannot find, one `gh` refuses
/// for authentication, and a GitLab link the deck does not look up. Each
/// shows its reason with the deck's fixed sentence; once the open pull
/// request's lookups fail, its last answer stays, reads stale and keeps its
/// `checked_at` through later failed lookups; a
/// rate-limited lookup reads `rate_limited` and holds back a reference named
/// after it. On a daemon whose `PATH` has no `gh`, a reference reads
/// `no_github_access`, and a bare `#N` in a folder with no GitHub origin
/// reads `unrecognised`.
#[spec("agent-report/refs/002")]
#[test]
fn refs_002_unchecked_references_say_why() {
    let deck = RefDeck::new(false);
    let other = deck.scratch.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();

    // Pending, then checked: a short handshake done before any other
    // reference is named or waited on.
    let first = deck.agent("refs-first", &deck.repo);
    let open = observe_pending_then_release(&deck, &first, &[50, 51, 52]);
    let fresh = wait_ref(
        &deck.daemon,
        &first,
        "pull-request",
        "checked",
        status_is("checked"),
    );
    assert_eq!(fresh["result"]["state"], "open", "{fresh:#}");
    assert_eq!(fresh["result"]["stale"], false, "{fresh:#}");

    // The open pull request is read again every second; its answer now
    // fails, so its last answer is kept and says it is stale. A lookup
    // already running when the answer is switched may still succeed and
    // move `checked_at`, so the test does not compare with the success it
    // saw above: it waits until a lookup that started after the switch has
    // been recorded, then checks that `checked_at` stays put across a
    // further failed lookup.
    deck.answer(
        "pr",
        ORIGIN_REPO,
        open,
        1,
        "",
        "error connecting to api.github.com\n",
    );
    let switched = deck.lookups_of("pr", ORIGIN_REPO, open);
    let stale = after_lookups(&deck, &first, open, switched + 2);
    assert_eq!(stale["result"]["status"], "checked", "{stale:#}");
    assert_eq!(stale["result"]["state"], "open", "{stale:#}");
    assert_eq!(stale["result"]["stale"], true, "{stale:#}");
    let checked_at = stale["result"]["checked_at"].clone();
    assert!(
        rfc3339(&checked_at) >= rfc3339(&fresh["result"]["checked_at"]),
        "a stale answer keeps the time of a successful check: {stale:#}"
    );
    // `seen + 2`: lookup `seen + 1` started after the card above was read,
    // and it is recorded once lookup `seen + 2` starts.
    let seen = deck.lookups_of("pr", ORIGIN_REPO, open);
    let later = after_lookups(&deck, &first, open, seen + 2);
    assert_eq!(later["result"]["stale"], true, "{later:#}");
    assert_eq!(later["result"]["state"], "open", "{later:#}");
    assert_eq!(
        later["result"]["checked_at"], checked_at,
        "a stale answer keeps the time it was checked across later failed lookups"
    );

    deck.answer(
        "issue",
        ORIGIN_REPO,
        404,
        1,
        "",
        "GraphQL: Could not resolve to an issue or pull request with the number of 404. (repository.issue)\n",
    );
    deck.answer(
        "pr",
        "locked-org/locked-repo",
        60,
        4,
        "",
        "To get started with GitHub CLI, please run:  gh auth login\n",
    );
    let second = deck.agent("refs-second", &other);
    publish(
        &deck.daemon,
        &first,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": format!("#{open}")},
            {"name": "issue", "value": "test-org/test-repo#404"}
        ]}),
    );
    publish(
        &deck.daemon,
        &second,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": "locked-org/locked-repo#60"},
            {"name": "issue", "value": "https://gitlab.com/some-org/some-repo/-/issues/3"}
        ]}),
    );

    let not_found = wait_ref(
        &deck.daemon,
        &first,
        "issue",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        not_found["result"],
        json!({"status": "unavailable", "reason": "not_found",
            "detail": "not found, or private to the deck's GitHub login"}),
        "{not_found:#}"
    );
    assert_eq!(
        not_found["url"],
        "https://github.com/test-org/test-repo/issues/404"
    );

    let auth = wait_ref(
        &deck.daemon,
        &second,
        "pull-request",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        auth["result"],
        json!({"status": "unavailable", "reason": "no_github_access",
            "detail": "gh is missing or not logged in on the deck's host"}),
        "{auth:#}"
    );
    let gitlab = wait_ref(
        &deck.daemon,
        &second,
        "issue",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        gitlab["result"],
        json!({"status": "unavailable", "reason": "unrecognised",
            "detail": "not a GitHub pull request or issue reference"}),
        "{gitlab:#}"
    );
    assert!(
        gitlab.get("url").is_none(),
        "an unrecognised value is not linked: {gitlab:#}"
    );
    assert!(
        !deck
            .lookups()
            .iter()
            .any(|c| c.contains("gitlab") || c.contains(" 3 ")),
        "a value that is not a GitHub reference is never looked up: {:?}",
        deck.lookups()
    );

    // Rate limited, last: the daemon holds reference lookups back after it.
    deck.answer(
        "pr",
        "busy-org/busy-repo",
        70,
        1,
        "",
        "HTTP 403: API rate limit exceeded for user ID 1. (https://api.github.com/graphql)\n",
    );
    let third = deck.agent("refs-third", &other);
    publish(
        &deck.daemon,
        &third,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": "busy-org/busy-repo#70"}
        ]}),
    );
    let limited = wait_ref(
        &deck.daemon,
        &third,
        "pull-request",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        limited["result"],
        json!({"status": "unavailable", "reason": "rate_limited",
            "detail": "GitHub rate limit reached on the deck's host"}),
        "{limited:#}"
    );
    deck.answer(
        "issue",
        "busy-org/busy-repo",
        71,
        0,
        &issue_json("busy-org/busy-repo", 71, "OPEN", "REFS-002 after the limit"),
        "",
    );
    publish(
        &deck.daemon,
        &third,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": "busy-org/busy-repo#70"},
            {"name": "issue", "value": "busy-org/busy-repo#71"}
        ]}),
    );
    let held = wait_ref(&deck.daemon, &third, "issue", "listed", |_| true);
    assert_eq!(held["result"], json!({"status": "pending"}), "{held:#}");
    // Three one-second refresh intervals, well inside the pause: polled
    // throughout, so a lookup or a result for it fails the test as soon as
    // it shows.
    let still_pending = || {
        checked_in(&list(&deck.daemon), &third, "issue")
            .is_some_and(|r| r["result"] == json!({"status": "pending"}))
    };
    let moved = common::wait_until(common::load_scaled(Duration::from_secs(3)), || {
        deck.lookups_of("issue", "busy-org/busy-repo", 71) > 0 || !still_pending()
    });
    assert!(
        !moved,
        "the reference named during the rate-limit pause was looked up or answered"
    );
    let held = checked_in(&list(&deck.daemon), &third, "issue").unwrap();
    assert_eq!(
        held["result"],
        json!({"status": "pending"}),
        "a reference named during the rate-limit pause waits: {held:#}"
    );
    assert!(
        !deck
            .lookups()
            .iter()
            .any(|c| c.starts_with("issue view 71 ")),
        "no gh call runs during the rate-limit pause: {:?}",
        deck.lookups()
    );

    // No gh on the daemon's host at all.
    let bare = RefDeck::new(true);
    let nowhere = bare.scratch.path().join("not-a-repo");
    std::fs::create_dir_all(&nowhere).unwrap();
    let in_repo = bare.agent("refs-no-gh", &bare.repo);
    let outside = bare.agent("refs-no-origin", &nowhere);
    publish(
        &bare.daemon,
        &in_repo,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": "#5"}
        ]}),
    );
    publish(
        &bare.daemon,
        &outside,
        &json!({"status": "working", "fields": [
            {"name": "issue", "value": "#5"}
        ]}),
    );
    let missing = wait_ref(
        &bare.daemon,
        &in_repo,
        "pull-request",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        missing["result"],
        json!({"status": "unavailable", "reason": "no_github_access",
            "detail": "gh is missing or not logged in on the deck's host"}),
        "{missing:#}"
    );
    assert_eq!(missing["repo"], ORIGIN_REPO, "{missing:#}");
    let no_origin = wait_ref(
        &bare.daemon,
        &outside,
        "issue",
        "unavailable",
        status_is("unavailable"),
    );
    assert_eq!(
        no_origin["result"],
        json!({"status": "unavailable", "reason": "unrecognised",
            "detail": "no GitHub origin for this agent's folder"}),
        "{no_origin:#}"
    );
}

/// Scenario: The daemon's stand-in `gh` says pull request
/// `test-org/test-repo#88` is merged. A client lists the cards from a shell
/// whose own `PATH` has a `gh` that records every call and answers nothing:
/// the client shows the merged state with the deck's link, and its own `gh`
/// was never run, so the check it shows is the one the daemon made on its
/// host.
#[spec("agent-report/refs/003")]
#[test]
fn refs_003_the_check_runs_on_the_daemons_host() {
    let deck = RefDeck::new(false);
    deck.answer(
        "pr",
        ORIGIN_REPO,
        88,
        0,
        &pr_json(
            ORIGIN_REPO,
            88,
            "MERGED",
            "APPROVED",
            json!([{"status": "COMPLETED", "conclusion": "SUCCESS", "name": "build"}]),
            "REFS-003 checked by the deck",
        ),
        "",
    );
    let client_bin = deck.scratch.path().join("client-bin");
    std::fs::create_dir_all(&client_bin).unwrap();
    executable(&client_bin.join("gh"), CLIENT_GH);
    let client_calls = deck.scratch.path().join("client-gh-calls");
    let client_path = format!("{}:/usr/bin:/bin", client_bin.display());
    let client_env = [("CLIENT_GH_CALLS", client_calls.to_str().unwrap())];

    let agent = deck.agent("refs-remote", &deck.repo);
    publish(
        &deck.daemon,
        &agent,
        &json!({"status": "working", "fields": [
            {"name": "pull-request", "value": "test-org/test-repo#88"}
        ]}),
    );

    let last = RefCell::new(Value::Null);
    let ok = common::wait_until(common::load_scaled(WAIT), || {
        let doc = list_with(&deck.daemon, &client_path, &client_env);
        let done = checked_in(&doc, &agent, "pull-request")
            .is_some_and(|r| r["result"]["status"] == "checked");
        *last.borrow_mut() = doc;
        done
    });
    assert!(
        ok,
        "the client never saw the deck's check: {:#}",
        last.borrow()
    );
    let pr = checked_in(&last.borrow(), &agent, "pull-request").unwrap();
    assert_eq!(pr["result"]["state"], "merged", "{pr:#}");
    assert_eq!(pr["result"]["title"], "REFS-003 checked by the deck");
    assert_eq!(pr["url"], "https://github.com/test-org/test-repo/pull/88");
    assert_eq!(
        deck.lookups(),
        [format!(
            "pr view 88 --repo {ORIGIN_REPO} --json {PR_ARGV_FIELDS}"
        )],
        "the daemon's gh made the lookup"
    );
    assert!(
        !client_calls.exists(),
        "the client ran its own gh: {}",
        std::fs::read_to_string(&client_calls).unwrap_or_default()
    );
}
