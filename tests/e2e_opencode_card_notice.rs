#![cfg(all(feature = "e2e", target_os = "linux"))]

//! PRD #2050 — the deck says on an OpenCode agent's card when it cannot fully
//! support how that OpenCode runs: OpenCode 2 on its shared background
//! service, an integration that never reports, and a version older than the
//! deck supports.
//!
//! The daemon decides from the pane's process tree: the topmost process whose
//! EXECUTABLE file name starts with `opencode` is the client, its version is
//! what `<executable> --version` prints, and a direct child running
//! `opencode serve --stdio` is the private server `--standalone` starts. A
//! `#!/bin/sh` script named `opencode` is not detected (its executable is the
//! shell), and OpenCode itself is not installed where lane 1 runs, so each
//! test builds a native stand-in with `cc` that answers `--version` with a
//! chosen version and, given `--standalone`, starts that private server
//! child. No model, credential or real OpenCode is involved, so CI runs these
//! for real rather than skipping them.
//!
//! Every pane is started the way a wrapper (`devbox run …`, a script) starts
//! OpenCode: the command is a shell script the deck cannot see into, declared
//! `agent_type = "opencode"` so the deck watches it.
//!
//! Linux only: the stand-in starts its private server through its own
//! absolute path, and the daemon's process sampling was measured on Linux
//! (`/proc/<pid>/exe`); the macOS sampling path is not exercised here.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use common::TuiDeck;
use serde_json::{Value, json};
use spec::spec;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What OpenCode 2.x and 1.x print for `--version` (measured on 2.2.5 and
/// 1.18.35), and the newest 1.x the deck does not support.
const V2: &str = "opencode v2.2.5";
const V1: &str = "1.18.35";
const V1_TOO_OLD: &str = "1.18.28";

/// The daemon's watch: it looks every five seconds, says "shared service"
/// once a 2.x client has been seen for ten, and "not working" once a client's
/// integration has been silent for forty-five (`src/opencode_mode.rs`).
const POLL: Duration = Duration::from_secs(5);
const SHARED_GRACE: Duration = Duration::from_secs(10);
const SILENT_GRACE: Duration = Duration::from_secs(45);

/// The start of each card sentence, which fits the notice row at this width;
/// the whole sentence is asserted through `daemon status --json`.
const SHARED_LINE: &str = "OpenCode 2 is using its shared background service";
const NOT_WORKING_LINE: &str = "the deck's OpenCode integration is not working";
const TOO_OLD_LINE: &str = "OpenCode 1.18.28 is older than the deck supports";

const SHARED_TEXT: &str = "OpenCode 2 is using its shared background service, so the deck cannot follow this agent: add --standalone to the agent's command, or run opencode service set disabled true, then restart the agent";
const NOT_WORKING_TEXT: &str = "the deck's OpenCode integration is not working for this agent: run dot-agent-deck hooks install --agent opencode, then restart the agent (dot-agent-deck docs troubleshooting has more)";
const TOO_OLD_TEXT: &str = "OpenCode 1.18.28 is older than the deck supports (1.18.29 or later): upgrade OpenCode, then restart the agent";

/// A native `opencode` at `dir/opencode` that prints `version` for
/// `--version`; otherwise prints a banner and stays up until its parent goes.
/// Given `--standalone`, it first starts `opencode serve --stdio --port 0` as
/// a child (itself again), the private server OpenCode 2's `--standalone`
/// runs; that child also stays up until its parent goes.
fn native_opencode(dir: &Path, version: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create the stand-in's directory");
    let binary = dir.join("opencode");
    let path = binary.to_str().expect("a UTF-8 stand-in path");
    assert!(
        !path.contains(['"', '\\', '\n']) && !version.contains(['"', '\\', '\n']),
        "C string literals: {path} {version}"
    );
    let source = dir.join("opencode-standin.c");
    std::fs::write(
        &source,
        format!(
            r#"#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void until_orphaned(pid_t parent) {{
    while (getppid() == parent) sleep(1);
}}

int main(int argc, char **argv) {{
    if (argc > 1 && (strcmp(argv[1], "--version") == 0 || strcmp(argv[1], "-v") == 0)) {{
        puts("{version}");
        return 0;
    }}
    pid_t parent = getppid();
    if (argc > 1 && strcmp(argv[1], "serve") == 0) {{
        until_orphaned(parent);
        return 0;
    }}
    for (int i = 1; i < argc; i++) {{
        if (strcmp(argv[i], "--standalone") == 0) {{
            if (fork() == 0) {{
                char *server[] = {{"opencode", "serve", "--stdio", "--port", "0", NULL}};
                execv("{path}", server);
                _exit(127);
            }}
            break;
        }}
    }}
    printf("OPENCODE STAND-IN {version}\n");
    fflush(stdout);
    until_orphaned(parent);
    return 0;
}}
"#
        ),
    )
    .expect("write the stand-in's source");
    let out = Command::new("cc")
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("cc on PATH to build the native OpenCode stand-in");
    assert!(
        out.status.success(),
        "building the OpenCode stand-in: {out:?}"
    );
    let reported = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("run the stand-in's --version");
    assert_eq!(
        String::from_utf8_lossy(&reported.stdout).trim(),
        version,
        "precondition: the stand-in reports the chosen version"
    );
    binary
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// A deck wide enough that each card's notice row holds the start of its
/// sentence.
fn deck() -> TuiDeck {
    let deck = TuiDeck::builder()
        .with_pty_size(200, 50)
        .launch_with_fixture("minimal");
    assert!(
        common::wait_until(common::child_boot_budget(), || deck
            .attach_socket_path()
            .exists()),
        "the deck's daemon must listen"
    );
    deck
}

/// How a stand-in pane runs.
struct Launch<'a> {
    name: &'a str,
    opencode: &'a Path,
    args: &'a [&'a str],
    /// Whether the wrapper reports one native OpenCode event (what the deck's
    /// plugin sends when a session starts) before OpenCode runs, the evidence
    /// that the integration works for this agent.
    reports: bool,
}

/// Start `launch` as a pane of `deck` through a wrapper script, declared an
/// OpenCode agent, and return its agent id once its OpenCode client runs.
fn start(deck: &TuiDeck, launch: &Launch) -> String {
    let dir = deck.workdir().join(launch.name);
    std::fs::create_dir_all(&dir).expect("create the pane's directory");
    let started = dir.join("opencode.started");
    let hook_exit = dir.join("hook.exit");
    let report = if launch.reports {
        format!(
            "printf '%s' '{{\"session_id\":\"{name}-session\",\"event\":\"session.created\",\"status\":\"idle\",\"cwd\":\"{cwd}\"}}' | {deck} hook --agent opencode; printf '%s' \"$?\" > {exit}\n",
            name = launch.name,
            cwd = dir.display(),
            deck = quote(common::contained_deck_binary()),
            exit = quote(&hook_exit),
        )
    } else {
        String::new()
    };
    let args: Vec<String> = launch.args.iter().map(|a| format!("'{a}'")).collect();
    let wrapper = dir.join("run-opencode.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n{report}: > {started}\n{opencode} {args}\n",
            started = quote(&started),
            opencode = quote(launch.opencode),
            args = args.join(" "),
        ),
    )
    .expect("write the wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("make the wrapper executable");
    let command = common::capability_export_command(&format!("exec {}", quote(&wrapper)));
    let reply = inbox::attach(
        deck,
        json!({"op": "start-agent", "command": command, "cwd": deck.workdir(),
            "agent_type": "open_code", "rows": 20, "cols": 100,
            "env": [["DOT_AGENT_DECK_PANE_ID", launch.name]],
            "display_name": launch.name}),
    );
    assert_eq!(reply["ok"], true, "{} spawn: {reply}", launch.name);
    let agent = reply["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{} spawn must return an agent id: {reply}", launch.name))
        .to_string();
    let boot = common::child_boot_budget();
    assert!(
        common::wait_until(boot, || started.exists()),
        "{}: the wrapper must reach OpenCode within {boot:?}",
        launch.name
    );
    if launch.reports {
        assert!(
            common::wait_until(boot, || std::fs::read_to_string(&hook_exit)
                .is_ok_and(|s| s == "0")),
            "{}: the wrapper's native OpenCode event must be accepted: {:?}",
            launch.name,
            std::fs::read_to_string(&hook_exit)
        );
    }
    agent
}

fn status_document(deck: &TuiDeck) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
        .args(["daemon", "status", "--json"])
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", deck.attach_socket_path())
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .env("HOME", deck.home_dir())
        .current_dir(deck.workdir())
        .output()
        .expect("run daemon status --json");
    assert!(
        output.status.success(),
        "daemon status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("daemon status JSON")
}

/// `agent`'s card notice as `daemon status --json` reports it (`Null` for
/// none), with the whole document for diagnostics.
fn agent_notice(deck: &TuiDeck, agent: &str) -> (Value, Value) {
    let document = status_document(deck);
    let record = document["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .find(|record| record["agent_id"] == agent)
        .cloned()
        .unwrap_or_else(|| panic!("no agent {agent} in daemon status: {document}"));
    (record["inbox"]["agent_notice"].clone(), document)
}

/// Wait up to `budget` for `agent`'s card notice to be `kind` with `text`.
fn wait_notice(deck: &TuiDeck, agent: &str, kind: &str, text: &str, budget: Duration) {
    let budget = common::load_scaled(budget);
    // `daemon status` is asked only once the cheap poll sees a notice.
    let found = common::wait_until(budget, || {
        has_notice(deck, agent) && agent_notice(deck, agent).0["kind"] == kind
    });
    let (notice, document) = agent_notice(deck, agent);
    assert!(
        found,
        "agent {agent}'s card notice must become {kind} within {budget:?}; status: {document}"
    );
    assert_eq!(
        notice["text"], text,
        "{kind}'s sentence; status: {document}"
    );
}

/// Whether the daemon's record of `agent` carries a card notice, read over the
/// attach socket (`ListAgents`), cheap enough to poll.
fn has_notice(deck: &TuiDeck, agent: &str) -> bool {
    common::agent_records_on(deck.attach_socket_path())
        .iter()
        .find(|record| record.id == agent)
        .is_some_and(|record| {
            serde_json::to_string(record)
                .expect("encode the agent record")
                .contains("\"agent_notice\"")
        })
}

/// `agent` carries no card notice from now until `until`.
fn no_notice_until(deck: &TuiDeck, agent: &str, until: Instant) {
    let window = until.saturating_duration_since(Instant::now());
    let appeared = common::wait_until(window, || has_notice(deck, agent));
    let (notice, document) = agent_notice(deck, agent);
    assert!(
        !appeared && notice.is_null(),
        "agent {agent} must carry no card notice for {window:?}; status: {document}"
    );
}

/// The rendered dashboard shows `line` on exactly one card.
fn shown_on_one_card(deck: &TuiDeck, line: &str) {
    let budget = common::load_scaled(Duration::from_secs(10));
    assert!(
        deck.wait_for_grid_predicate_within(budget, |grid| grid.matches(line).count() == 1),
        "exactly one card must show {line:?}:\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Start two OpenCode 2 panes through wrapper scripts, one running `opencode` on the shared background service and one running `opencode --standalone`. After the deck's grace, the shared one's card says OpenCode 2 is using its shared background service and how to fix it, and the standalone one's card says nothing over the same window.
#[spec("inbox/opencode/061")]
#[test]
fn opencode_061_opencode_2_on_the_shared_service_is_named_on_its_card_and_standalone_is_not() {
    let deck = deck();
    let opencode = native_opencode(&deck.home_dir().join("opencode-v2"), V2);
    let standalone = start(
        &deck,
        &Launch {
            name: "oc-standalone",
            opencode: &opencode,
            args: &["--standalone"],
            reports: true,
        },
    );
    let shared = start(
        &deck,
        &Launch {
            name: "oc-shared",
            opencode: &opencode,
            args: &[],
            reports: true,
        },
    );
    let started = Instant::now();
    wait_notice(
        &deck,
        &shared,
        "opencode_shared_service",
        SHARED_TEXT,
        SHARED_GRACE + 4 * POLL,
    );
    shown_on_one_card(&deck, SHARED_LINE);
    // The standalone pane has been watched at least as long as the shared one
    // needed, plus two more looks.
    no_notice_until(&deck, &standalone, started + SHARED_GRACE + 2 * POLL);
    shown_on_one_card(&deck, SHARED_LINE);
}

/// Scenario: Start two `opencode --standalone` panes through wrapper scripts: one whose integration never reports to the deck, as when OpenCode cannot find the deck's plugin, and one that reports once as it starts and then sits idle. After the deck's grace, the silent one's card says the deck's OpenCode integration is not working and how to fix it, and the idle healthy one's card says nothing over the same window.
#[spec("inbox/opencode/062")]
#[test]
fn opencode_062_a_silent_integration_is_named_on_its_card_and_an_idle_healthy_one_is_not() {
    let deck = deck();
    let opencode = native_opencode(&deck.home_dir().join("opencode-v2"), V2);
    let healthy = start(
        &deck,
        &Launch {
            name: "oc-healthy",
            opencode: &opencode,
            args: &["--standalone"],
            reports: true,
        },
    );
    let silent = start(
        &deck,
        &Launch {
            name: "oc-silent",
            opencode: &opencode,
            args: &["--standalone"],
            reports: false,
        },
    );
    let started = Instant::now();
    // Nothing is said while the grace runs.
    no_notice_until(&deck, &silent, started + SILENT_GRACE - 2 * POLL);
    wait_notice(
        &deck,
        &silent,
        "opencode_not_reporting",
        NOT_WORKING_TEXT,
        5 * POLL,
    );
    shown_on_one_card(&deck, NOT_WORKING_LINE);
    no_notice_until(
        &deck,
        &healthy,
        Instant::now() + POLL + Duration::from_secs(1),
    );
    shown_on_one_card(&deck, NOT_WORKING_LINE);
}

/// Scenario: Start two OpenCode 1.x panes through wrapper scripts, one running 1.18.28 and one running a supported 1.18.35 without `--standalone`. The 1.18.28 card at once says that OpenCode version is older than the deck supports and to upgrade, and the 1.18.35 card says nothing, since OpenCode 1 has no shared service.
#[spec("inbox/opencode/063")]
#[test]
fn opencode_063_an_opencode_older_than_the_floor_is_named_on_its_card_at_once() {
    let deck = deck();
    let too_old = native_opencode(&deck.home_dir().join("opencode-old"), V1_TOO_OLD);
    let supported = native_opencode(&deck.home_dir().join("opencode-v1"), V1);
    let fine = start(
        &deck,
        &Launch {
            name: "oc-supported",
            opencode: &supported,
            args: &[],
            reports: true,
        },
    );
    let old = start(
        &deck,
        &Launch {
            name: "oc-too-old",
            opencode: &too_old,
            args: &[],
            reports: true,
        },
    );
    let started = Instant::now();
    // "At once": on the first looks after the client appears, well inside the
    // grace any other notice waits for.
    wait_notice(
        &deck,
        &old,
        "opencode_unsupported_version",
        TOO_OLD_TEXT,
        2 * POLL + Duration::from_secs(1),
    );
    shown_on_one_card(&deck, TOO_OLD_LINE);
    no_notice_until(&deck, &fine, started + SHARED_GRACE + 2 * POLL);
    shown_on_one_card(&deck, TOO_OLD_LINE);
}
