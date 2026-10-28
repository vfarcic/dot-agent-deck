#![cfg(all(feature = "e2e", unix))]

//! PRD #190 — an OpenCode agent the daemon starts with no TUI attached loads a
//! plugin that names the running deck, even when the plugin went stale, or
//! OpenCode was set up, after the daemon started.
//!
//! `daemon serve` installs the plugin when it starts (issue #1157,
//! `hooks/install/009`), and runs that install again before every OpenCode
//! agent it starts (PRD #1666 §11), keeping a pin to another live install. `inbox/boundary/025` covers that per-start
//! rewrite for the template, around a pin the deck keeps; this file covers the
//! pin itself on the path PRD #190 names: a scheduled task firing on a daemon
//! no TUI has touched, after the plugin was left pointing at a copy of the deck
//! that no longer exists.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use spec::spec;

const WAIT: Duration = Duration::from_secs(20);

/// The `const BINARY_PATH = …;` line the plugin holds when it pins `binary`.
fn pin_line(binary: &Path) -> String {
    format!(
        "const BINARY_PATH = {};",
        serde_json::to_string(&binary.to_string_lossy()).expect("encode the pin")
    )
}

/// Scenario: Start a headless `daemon serve` in a HOME with no OpenCode
/// folder, so its startup install writes no plugin. Then set OpenCode up in
/// both layouts (`~/.config/opencode` and `~/.opencode`), each holding a deck
/// plugin pinned to a copy of the deck that has since been deleted, and fire a
/// scheduled task whose command is an OpenCode stand-in that records the
/// plugins' pins the moment it starts. Both plugins must already name the
/// daemon's own install, not the deleted copy, with no TUI ever started.
#[spec("hooks/install/023")]
#[test]
fn install_023_a_scheduled_opencode_agent_loads_a_plugin_pinned_to_the_daemon() {
    let scratch = common::harness_tempdir().expect("scratch tempdir");
    let work = scratch.path();
    let bin = work.join("oc-bin");
    std::fs::create_dir_all(&bin).expect("create the stand-in's bindir");
    let standin = bin.join("opencode");
    let seen = work.join("opencode.seen");

    let toml = format!(
        "[[scheduled_tasks]]\n\
         name = \"plugin-refresh\"\n\
         cron = \"0 0 1 1 *\"\n\
         working_dir = \"{work}\"\n\
         command = \"{command}\"\n\
         prompt = \"PRD-190-PLUGIN-REFRESH\"\n\
         enabled = true\n",
        work = work.display(),
        command = standin.display(),
    );
    let daemon = common::spawn_daemon_serve(Some(&toml), "0");

    let roots = [
        daemon.home.join(".config").join("opencode"),
        daemon.home.join(".opencode"),
    ];
    let plugins: Vec<_> = roots
        .iter()
        .map(|root| root.join("plugin").join("dot-agent-deck.js"))
        .collect();
    assert!(
        plugins.iter().all(|plugin| !plugin.exists()),
        "precondition: with no OpenCode folder, the daemon's startup install writes no plugin"
    );

    // OpenCode set up after the daemon started, with plugins a since-deleted
    // copy of the deck left behind.
    let deleted = daemon
        .home
        .join("removed-install")
        .join("bin")
        .join("dot-agent-deck");
    let stale = format!("{}\n", pin_line(&deleted));
    for plugin in &plugins {
        std::fs::create_dir_all(plugin.parent().unwrap()).expect("create the plugin dir");
        std::fs::write(plugin, &stale).expect("seed the stale plugin");
    }

    // A stand-in OpenCode that records what each plugin pins as it starts,
    // then waits to be reaped with the daemon's process group.
    let quoted: Vec<_> = plugins
        .iter()
        .map(|plugin| format!("'{}'", plugin.display()))
        .collect();
    std::fs::write(
        &standin,
        format!(
            "#!/bin/sh\ngrep -H 'BINARY_PATH = ' {plugins} > '{seen}.tmp'\nmv '{seen}.tmp' '{seen}'\nexec sleep 600\n",
            plugins = quoted.join(" "),
            seen = seen.display(),
        ),
    )
    .expect("write the stand-in");
    std::fs::set_permissions(&standin, std::fs::Permissions::from_mode(0o755))
        .expect("make the stand-in executable");

    daemon
        .run_now("plugin-refresh")
        .expect("run the scheduled task now");
    assert!(
        common::wait_until(WAIT, || seen.is_file()),
        "the scheduled OpenCode stand-in never started; agents: {:?}",
        daemon.agent_records()
    );
    let read = std::fs::read_to_string(&seen).expect("read what the stand-in saw");

    let expected = pin_line(
        &daemon
            .home
            .join(".local")
            .join("bin")
            .join("dot-agent-deck"),
    );
    for plugin in &plugins {
        let line = read
            .lines()
            .find(|line| line.starts_with(&format!("{}:", plugin.display())))
            .unwrap_or_else(|| panic!("no pin recorded for {}:\n{read}", plugin.display()));
        assert!(
            line.ends_with(&expected),
            "the agent loaded {} pinned to something other than the daemon's install \
             (`{expected}`):\n{read}",
            plugin.display()
        );
    }
}
