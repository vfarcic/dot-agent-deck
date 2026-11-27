#![cfg(all(feature = "e2e", unix))]

//! Credential-free coverage of `remote upgrade --all` (issue #1600): several
//! sandboxed remotes on one deck list, each with its own HOME, daemon and
//! installer, reached through one SSH shim that routes by host. The old and new
//! daemons run the same retained Cargo build; an old one reports an older
//! release through `DOT_AGENT_DECK_DAEMON_VERSION_OVERRIDE`, as a previous
//! release's daemon would. This proves the batch's policy and process
//! replacement across daemons, not release compatibility or real SSH.

dot_agent_deck_test_harness::link!();

use std::fs;

use spec::spec;

#[path = "support/remote_upgrade.rs"]
mod upgrade_fixture;
use upgrade_fixture::*;

/// The row a deck list holds for a remote nothing answers for: the fleet's
/// SSH shim refuses its host as an unreachable machine would.
fn unreachable_row(name: &str) -> String {
    format!(
        "\n[[remotes]]\nname = \"{name}\"\ntype = \"ssh\"\nhost = \"{name}.invalid\"\nport = 22\nversion = \"{OLD_VERSION}\"\nadded_at = \"2026-10-03T00:00:00Z\"\n"
    )
}

/// The line the final summary prints for `deck`.
fn summary_line<'a>(text: &'a str, deck: &str) -> &'a str {
    let prefix = format!("  {deck}: ");
    text.lines()
        .rev()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("the summary must have a line for {deck}; output:\n{text}"))
}

/// Scenario: Run `remote upgrade --all` in a terminal against three remotes: an idle one and a busy one that run an older release, and one already current. The idle daemon restarts without a question, the busy one asks once naming its own agents and roles and Enter keeps it, the current one is skipped, and one line per deck ends the run.
#[spec("remote/upgrade/009")]
#[test]
fn remote_upgrade_009_all_upgrades_each_older_daemon_and_asks_per_daemon() {
    let idle = Remote::named("alpha", "alpha.invalid", true);
    let mut busy = Remote::named("beta", "beta.invalid", true);
    busy.seed_live();
    let current = Remote::named("gamma", "gamma.invalid", false);
    let mut fleet = Fleet::new(vec![idle, busy, current]);

    let mut terminal = fleet.tty(&["remote", "upgrade", "--all"]);
    terminal.wait("Checked 3 remote decks: 2 to upgrade.");
    terminal.wait("Restart now?");
    let asked = terminal.output();
    // The idle deck comes first on the list and was restarted before the busy
    // one asked anything.
    assert!(
        asked.contains("Restarted the daemon on 'alpha'"),
        "the idle daemon must restart, without a question, before the next deck: {asked}"
    );
    assert!(
        asked.contains("Restarting the daemon on 'beta' onto the new build stops:"),
        "the question must name the deck it is about: {asked}"
    );
    for label in LABELS {
        assert!(
            asked.contains(label),
            "the question must name {label}: {asked}"
        );
    }
    assert!(
        asked.contains("upgrade-team") && asked.contains("Keep current daemon"),
        "the question must name the orchestration and the safe default: {asked}"
    );
    fleet.remotes[1].assert_installed();
    fleet.remotes[1].assert_kept();

    terminal.send("\n");
    terminal.success();
    let text = terminal.output();
    assert_eq!(
        text.matches("Restart now?").count(),
        1,
        "only the busy daemon asks, and only once: {text}"
    );
    assert!(
        text.contains("Upgrade all: 1 upgraded, 1 kept running, 1 skipped of 3 remote decks."),
        "the run must end with the counts: {text}"
    );
    assert!(summary_line(&text, "alpha").contains("Upgraded: now runs"));
    assert!(summary_line(&text, "beta").contains("Kept running: installed"));
    assert!(summary_line(&text, "beta").contains("as you chose"));
    assert!(summary_line(&text, "gamma").contains("Skipped: already runs"));

    fleet.remotes[0].assert_installed();
    fleet.remotes[0].assert_restarted();
    fleet.remotes[1].assert_kept();
    // The current deck was only checked: nothing was installed or restarted.
    let gamma = &mut fleet.remotes[2];
    assert!(
        !gamma.dir.path().join("hooks-installed").exists(),
        "a current deck must not be installed over"
    );
    assert!(
        gamma.child.try_wait().unwrap().is_none(),
        "a current deck's daemon must keep running"
    );
}

/// Scenario: Pipe `remote upgrade --all --json` over a deck list whose first deck fails to install, whose second cannot be reached, and whose third is an older idle daemon. The failures do not stop the batch: the third deck is still upgraded, stdout holds one report naming every deck, and the command exits non-zero.
#[spec("remote/upgrade/010")]
#[test]
fn remote_upgrade_010_a_failed_deck_does_not_stop_the_batch_and_exits_non_zero() {
    let broken = Remote::named("alpha", "alpha.invalid", true);
    fs::write(broken.dir.path().join("fail-install"), "fail").unwrap();
    let idle = Remote::named("gamma", "gamma.invalid", true);
    let mut fleet = Fleet::new(vec![broken, idle]);
    // An unreachable deck between the two, in deck-list order.
    let registry = fleet.remotes[0].registry.clone();
    let rows = fs::read_to_string(&registry).unwrap();
    let (first, rest) = rows.split_at(rows.find("\n[[remotes]]").expect("two rows"));
    fs::write(
        &registry,
        format!("{first}{}{rest}", unreachable_row("beta")),
    )
    .unwrap();

    let output = fleet.piped(&["remote", "upgrade", "--all", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a failed deck must make the batch exit non-zero; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout must be one JSON report ({e}); stdout:\n{stdout}\nstderr:\n{stderr}")
    });
    let decks = report["decks"].as_array().expect("a deck per row");
    let seen: Vec<(&str, &str)> = decks
        .iter()
        .map(|d| (d["deck"].as_str().unwrap(), d["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        seen,
        [
            ("alpha", "failed"),
            ("beta", "failed"),
            ("gamma", "upgraded")
        ],
        "every deck is reported in deck-list order; stderr:\n{stderr}"
    );
    assert_eq!(decks[0]["result"]["outcome"]["stage"], "installing");
    assert_eq!(decks[1]["result"]["kind"], "unreachable");
    assert!(
        decks[1]["line"]
            .as_str()
            .unwrap()
            .starts_with("Failed: it could not be checked:"),
        "{}",
        decks[1]["line"]
    );
    fleet.remotes[1].assert_installed();
    fleet.remotes[1].assert_restarted();
    // The deck whose install failed keeps its daemon.
    let alpha = &mut fleet.remotes[0];
    assert!(
        alpha.child.try_wait().unwrap().is_none(),
        "a failed install must leave its daemon running"
    );
}
