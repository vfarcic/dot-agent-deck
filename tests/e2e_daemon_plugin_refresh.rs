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
    // then waits to be reaped with the daemon's process group. It answers
    // `--version` (as a 1.x OpenCode, the layout this test was written for)
    // and exits before recording: the deck runs `<opencode> --version` before
    // each OpenCode spawn, ahead of the per-spawn install (PRD #2050), so a
    // stand-in that recorded on that probe would record the stale pins, and
    // one that slept on it would stall the spawn on the probe's bound.
    let quoted: Vec<_> = plugins
        .iter()
        .map(|plugin| format!("'{}'", plugin.display()))
        .collect();
    std::fs::write(
        &standin,
        format!(
            "#!/bin/sh\ncase \"$1\" in --version|-v) echo '{V1}'; exit 0;; esac\ngrep -H 'BINARY_PATH = ' {plugins} > '{seen}.tmp'\nmv '{seen}.tmp' '{seen}'\nexec sleep 600\n",
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

// ---------------------------------------------------------------------------
// PRD #2050 — the plugin layout follows the OpenCode version an agent runs.
//
// Each test below starts OpenCode agents from scheduled tasks on a headless
// `daemon serve`, through a stand-in `opencode` that answers `--version` with
// a chosen version and records its argv and the layout of the deck's OpenCode
// artifacts the moment it starts. The deck detects the version by running the
// resolved binary's `--version` before each OpenCode agent starts, and caches
// it per binary file, so a version switch here REPLACES the stand-in (a new
// file at the same path, as an installer does) rather than editing it in place.
// ---------------------------------------------------------------------------

/// What `opencode --version` prints on each major version (measured
/// 2026-10-31 on 2.2.5 and 1.18.35).
const V2: &str = "opencode v2.2.5";
const V1: &str = "1.18.35";

/// The deck artifacts whose presence the version decides.
struct Layout {
    /// The server module in the global config root, `~/.config/opencode`.
    global: std::path::PathBuf,
    /// The server module in the legacy root, `~/.opencode`.
    legacy: std::path::PathBuf,
    /// OpenCode 2.x's TUI half, a plugin directory in the global root.
    tui_v2: std::path::PathBuf,
}

struct VersionedOpenCode {
    daemon: common::DaemonProc,
    standin: std::path::PathBuf,
    spawns: std::path::PathBuf,
    layout: Layout,
    // Last, so the daemon (and the stand-ins it reaps) go before their files.
    _scratch: tempfile::TempDir,
}

impl VersionedOpenCode {
    /// A daemon with `phases` scheduled tasks (`phase-1`…) whose command is the
    /// stand-in, and both OpenCode roots created AFTER the daemon started, so
    /// its startup install writes nothing and every artifact below comes from
    /// the per-spawn install this file is about.
    fn new(phases: usize) -> Self {
        let tempdir = common::harness_tempdir().expect("scratch tempdir");
        let scratch = tempdir.path().to_path_buf();
        let bin = scratch.join("oc-bin");
        std::fs::create_dir_all(&bin).expect("create the stand-in's bindir");
        let standin = bin.join("opencode");
        let spawns = scratch.join("spawns");
        std::fs::create_dir_all(&spawns).expect("create the spawn record dir");
        let toml: String = (1..=phases)
            .map(|n| {
                format!(
                    "[[scheduled_tasks]]\n\
                     name = \"phase-{n}\"\n\
                     cron = \"0 0 1 1 *\"\n\
                     working_dir = \"{work}\"\n\
                     command = \"{command}\"\n\
                     prompt = \"PRD-2050-PHASE-{n}\"\n\
                     enabled = true\n",
                    work = scratch.display(),
                    command = standin.display(),
                )
            })
            .collect();
        // The stand-in's directory first on the daemon's PATH as well, so any
        // `opencode` the deck resolves by name is the stand-in, never a real
        // OpenCode installed on the host.
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let daemon = common::spawn_daemon_serve_with_env(Some(&toml), "0", &[("PATH", &path)]);
        let global_root = daemon.home.join(".config").join("opencode");
        let legacy_root = daemon.home.join(".opencode");
        assert!(
            !global_root.join("plugin").exists() && !legacy_root.join("plugin").exists(),
            "precondition: with no OpenCode folder, the daemon's startup install writes no plugin"
        );
        std::fs::create_dir_all(&global_root).expect("set up OpenCode's global config root");
        std::fs::create_dir_all(&legacy_root).expect("set up OpenCode's legacy root");
        let layout = Layout {
            global: global_root.join("plugin").join("dot-agent-deck.js"),
            legacy: legacy_root.join("plugin").join("dot-agent-deck.js"),
            tui_v2: global_root
                .join("plugins")
                .join("dot-agent-deck-tui")
                .join("tui.js"),
        };
        Self {
            daemon,
            standin,
            spawns,
            layout,
            _scratch: tempdir,
        }
    }

    /// Install a stand-in reporting `version`, replacing any earlier one with a
    /// new file (new inode, new content), the way an OpenCode installer
    /// replaces the binary on an upgrade or downgrade.
    fn install(&self, version: &str) {
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in --version|-v) echo '{version}'; exit 0;; esac\n\
             dir='{spawns}'\n\
             n=$(ls \"$dir\" | grep -c '^spawn-')\n\
             {{ printf 'version={version}\\n'; for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done; \
             for f in '{global}' '{legacy}' '{tui}'; do if [ -e \"$f\" ]; then printf 'present=%s\\n' \"$f\"; fi; done; }} > \"$dir/tmp-$$\"\n\
             mv \"$dir/tmp-$$\" \"$dir/spawn-$n\"\n\
             exec sleep 600\n",
            spawns = self.spawns.display(),
            global = self.layout.global.display(),
            legacy = self.layout.legacy.display(),
            tui = self.layout.tui_v2.display(),
        );
        let fresh = self.standin.with_extension("new");
        std::fs::write(&fresh, script).expect("write the stand-in");
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))
            .expect("make the stand-in executable");
        std::fs::rename(&fresh, &self.standin).expect("replace the stand-in");
    }

    /// Fire `phase-<n>` and return what its OpenCode stand-in recorded as it
    /// started: one `key=value` per line (`version=`, `arg=`, `present=`).
    fn spawn(&self, phase: usize) -> Vec<String> {
        let before = self.records();
        self.daemon
            .run_now(&format!("phase-{phase}"))
            .expect("run the scheduled task now");
        let record = self.spawns.join(format!("spawn-{before}"));
        assert!(
            common::wait_until(WAIT, || record.is_file()),
            "the scheduled OpenCode stand-in for phase-{phase} never started; agents: {:?}",
            self.daemon.agent_records()
        );
        std::fs::read_to_string(&record)
            .expect("read what the stand-in saw")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn records(&self) -> usize {
        std::fs::read_dir(&self.spawns)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().starts_with("spawn-"))
                    .count()
            })
            .unwrap_or(0)
    }
}

/// How many times the agent's argv carries `--standalone`.
fn standalone_count(record: &[String]) -> usize {
    record
        .iter()
        .filter(|line| line.as_str() == "arg=--standalone")
        .count()
}

/// Whether the stand-in saw `path` present when it started.
fn saw(record: &[String], path: &Path) -> bool {
    record
        .iter()
        .any(|line| line.strip_prefix("present=") == Some(&*path.to_string_lossy()))
}

/// The default export of the module at `path`, imported under Node the way
/// OpenCode imports a local plugin: its type, its `id`, and which of `server`
/// (1.x) and `setup` (2.x) are functions.
fn default_export(path: &Path) -> serde_json::Value {
    let script = "const {pathToFileURL}=await import('node:url');\
        const d=(await import(pathToFileURL(process.argv[1]).href)).default;const m=d??{};\
        console.log(JSON.stringify({default:typeof d,id:m.id??null,server:typeof m.server,setup:typeof m.setup}));\
        process.exit(0);";
    let output = std::process::Command::new("node")
        .args(["--input-type=module", "-e", script])
        .arg(path)
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "node could not import {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{}: {e}: {stdout}", path.display()))
}

/// The OpenCode 2.x layout: one dual-shape server module in the global root,
/// none in the legacy root, the 2.x TUI half present, `--standalone` once.
fn assert_v2_layout(oc: &VersionedOpenCode, record: &[String], when: &str) {
    let layout = &oc.layout;
    assert!(
        saw(record, &layout.global),
        "{when}: an OpenCode 2.x agent must start with the deck's server module at {}; \
         the stand-in saw:\n{}",
        layout.global.display(),
        record.join("\n")
    );
    assert!(
        !saw(record, &layout.legacy),
        "{when}: OpenCode 2.x scans the legacy root too and fails a second copy with \
         `Duplicate plugin ID`, so {} must be absent; the stand-in saw:\n{}",
        layout.legacy.display(),
        record.join("\n")
    );
    assert!(
        saw(record, &layout.tui_v2),
        "{when}: an OpenCode 2.x agent must start with the 2.x TUI half at {}; \
         the stand-in saw:\n{}",
        layout.tui_v2.display(),
        record.join("\n")
    );
    assert_eq!(
        standalone_count(record),
        1,
        "{when}: a visible `opencode` command on 2.x must run with `--standalone`, once; \
         the stand-in saw:\n{}",
        record.join("\n")
    );
    assert_eq!(
        default_export(&layout.global),
        serde_json::json!({"default": "object", "id": "dot-agent-deck", "server": "function", "setup": "function"}),
        "{when}: the server module must default-export both shapes, \
         `{{ id, server, setup }}`, so 1.x and 2.x each load their own half"
    );
    let tui = default_export(&layout.tui_v2);
    assert_eq!(
        tui["setup"], "function",
        "{when}: the 2.x TUI half must default-export `{{ id, setup }}`: {tui}"
    );
    assert!(
        tui["id"].is_string(),
        "{when}: the 2.x TUI half needs an id: {tui}"
    );
}

/// The OpenCode 1.x layout as it ships today: the server module in both roots,
/// no 2.x TUI half, and no `--standalone`.
fn assert_v1_layout(oc: &VersionedOpenCode, record: &[String], when: &str) {
    let layout = &oc.layout;
    for plugin in [&layout.global, &layout.legacy] {
        assert!(
            saw(record, plugin),
            "{when}: an OpenCode 1.x agent must start with the deck's server module at {}; \
             the stand-in saw:\n{}",
            plugin.display(),
            record.join("\n")
        );
        // 1.x loads either shape: a default-exported function (the module as
        // it shipped before PRD #2050) or `{ id, server }` (1.18.29+, and the
        // dual-shape module).
        let export = default_export(plugin);
        let loadable = export["default"] == "function"
            || (export["server"] == "function" && export["id"].is_string());
        assert!(
            loadable,
            "{when}: OpenCode 1.x must be able to load {}: {export}",
            plugin.display()
        );
    }
    assert!(
        !saw(record, &layout.tui_v2),
        "{when}: no 2.x TUI half on 1.x; the stand-in saw {}",
        layout.tui_v2.display()
    );
    assert_eq!(
        standalone_count(record),
        0,
        "{when}: 1.x has no `--standalone`, so the deck must never append it; \
         the stand-in saw:\n{}",
        record.join("\n")
    );
}

/// Scenario: Start a headless daemon, then set OpenCode up in both roots and
/// start an OpenCode agent from a scheduled task whose stand-in `opencode`
/// reports `opencode v2.2.5`. As it starts, the deck's dual-shape server module
/// must be in the global root only, the 2.x TUI half must be installed, and
/// the agent's command line must carry `--standalone` exactly once.
#[spec("hooks/install/025")]
#[test]
fn install_025_an_opencode_2_agent_starts_with_the_2x_layout_and_standalone() {
    let oc = VersionedOpenCode::new(1);
    oc.install(V2);
    let record = oc.spawn(1);
    assert_v2_layout(&oc, &record, "first OpenCode 2.x spawn");
}

/// Scenario: Start a headless daemon, then set OpenCode up in both roots and
/// start an OpenCode agent from a scheduled task whose stand-in `opencode`
/// reports `1.18.35`. The deck's server module must be in both roots as it is
/// today, with no 2.x TUI half, and the command line must carry no
/// `--standalone`.
#[spec("hooks/install/026")]
#[test]
fn install_026_an_opencode_1_agent_keeps_todays_layout_and_no_standalone() {
    let oc = VersionedOpenCode::new(1);
    oc.install(V1);
    let record = oc.spawn(1);
    assert_v1_layout(&oc, &record, "OpenCode 1.x spawn");
}

/// Scenario: Start an OpenCode agent on a 1.x stand-in, replace the stand-in
/// with a 2.x one (as an upgrade does) and start another, then go back to 1.x
/// and start a third. Each agent must find the layout of the version it runs:
/// the upgrade removes the legacy-root duplicate and adds the 2.x TUI half and
/// `--standalone`, and the downgrade restores the 1.x layout and drops both.
#[spec("hooks/install/027")]
#[test]
fn install_027_switching_opencode_versions_repairs_the_layout_on_the_next_spawn() {
    let oc = VersionedOpenCode::new(3);
    oc.install(V1);
    let first = oc.spawn(1);
    assert_v1_layout(&oc, &first, "before the upgrade (1.x)");

    oc.install(V2);
    let upgraded = oc.spawn(2);
    assert_v2_layout(&oc, &upgraded, "after upgrading to 2.x");

    oc.install(V1);
    let downgraded = oc.spawn(3);
    assert_v1_layout(&oc, &downgraded, "after going back to 1.x");
}
