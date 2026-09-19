#![cfg(all(feature = "e2e", unix))]

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TuiDeck;
use dot_agent_deck::platform::paths::TEST_LEGACY_ENDPOINT_ROOT_ENV;
use spec::spec;

const DASHBOARD_EMPTY_STATE: &str = "No active sessions";
const LEGACY_SQUATTER_MARKER: &str = "legacy endpoint squatter\n";

struct EndpointPaths {
    dir: PathBuf,
    hook: PathBuf,
    attach: PathBuf,
    temp_legacy_attach: PathBuf,
}

impl EndpointPaths {
    fn under(temp_dir: &Path) -> Self {
        let uid = current_uid();
        let dir = temp_dir.join(format!("dot-agent-deck-{uid}"));
        Self {
            hook: dir.join("hook.sock"),
            attach: dir.join("attach.sock"),
            dir,
            temp_legacy_attach: temp_dir.join(format!("dot-agent-deck-attach-{uid}.sock")),
        }
    }
}

struct LegacyEndpointPaths {
    root: PathBuf,
    hook: PathBuf,
    attach: PathBuf,
}

impl LegacyEndpointPaths {
    fn under(root: &Path) -> Self {
        let uid = current_uid();
        Self {
            root: root.to_path_buf(),
            hook: root.join(format!("dot-agent-deck-{uid}.sock")),
            attach: root.join(format!("dot-agent-deck-attach-{uid}.sock")),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum EndpointSnapshot {
    Missing,
    Present {
        device: u64,
        inode: u64,
        mode: u32,
        links: u64,
        uid: u32,
        gid: u32,
        rdev: u64,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        changed_seconds: i64,
        changed_nanoseconds: i64,
    },
}

impl EndpointSnapshot {
    fn capture(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Self::Present {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                links: metadata.nlink(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                rdev: metadata.rdev(),
                size: metadata.size(),
                modified_seconds: metadata.mtime(),
                modified_nanoseconds: metadata.mtime_nsec(),
                changed_seconds: metadata.ctime(),
                changed_nanoseconds: metadata.ctime_nsec(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(error) => panic!(
                "inspect literal legacy endpoint {}: {error}",
                path.display()
            ),
        }
    }
}

struct LiteralLegacyEndpoints {
    entries: [(PathBuf, EndpointSnapshot); 2],
}

impl LiteralLegacyEndpoints {
    fn capture() -> Self {
        let uid = current_uid();
        let hook = PathBuf::from(format!("/tmp/dot-agent-deck-{uid}.sock"));
        let attach = PathBuf::from(format!("/tmp/dot-agent-deck-attach-{uid}.sock"));
        Self {
            entries: [
                (hook.clone(), EndpointSnapshot::capture(&hook)),
                (attach.clone(), EndpointSnapshot::capture(&attach)),
            ],
        }
    }

    fn assert_unchanged(&self) {
        for (path, before) in &self.entries {
            let after = EndpointSnapshot::capture(path);
            assert_eq!(
                &after,
                before,
                "literal production endpoint {} changed during an isolated endpoint-fallback scenario",
                path.display()
            );
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and simply returns the caller's
    // effective uid.
    unsafe { libc::geteuid() }
}

fn launch_fallback_deck(temp_dir: &Path, legacy: &LegacyEndpointPaths, log_path: &Path) -> TuiDeck {
    TuiDeck::builder()
        .without_endpoint_overrides()
        .with_env("TMPDIR", temp_dir.to_string_lossy())
        .with_env(TEST_LEGACY_ENDPOINT_ROOT_ENV, legacy.root.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30")
        .launch_with_fixture("minimal")
}

fn inspect_new_endpoints(
    paths: &EndpointPaths,
    legacy: &LegacyEndpointPaths,
    legacy_must_be_absent: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    match fs::metadata(&paths.dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                failures.push(format!(
                    "fallback endpoint root {} is not a directory",
                    paths.dir.display()
                ));
            }
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o700 {
                failures.push(format!(
                    "fallback endpoint root {} has mode 0o{mode:o}, expected 0o700",
                    paths.dir.display()
                ));
            }
        }
        Err(error) => failures.push(format!(
            "fallback endpoint root {} was not created: {error}",
            paths.dir.display()
        )),
    }

    for endpoint in [&paths.attach, &paths.hook] {
        match fs::symlink_metadata(endpoint) {
            Ok(metadata) if metadata.file_type().is_socket() => {}
            Ok(metadata) => failures.push(format!(
                "fallback endpoint {} exists but is not a Unix socket ({:?})",
                endpoint.display(),
                metadata.file_type()
            )),
            Err(error) => failures.push(format!(
                "fallback endpoint {} is missing: {error}",
                endpoint.display()
            )),
        }
    }

    if paths.temp_legacy_attach.exists() {
        failures.push(format!(
            "legacy attach spelling was created under TMPDIR at {}",
            paths.temp_legacy_attach.display()
        ));
    }
    if legacy_must_be_absent && legacy.attach.exists() {
        failures.push(format!(
            "legacy attach spelling was created at {}",
            legacy.attach.display()
        ));
    }
    failures
}

fn stop_resolved_daemon(
    temp_dir: &Path,
    legacy: &LegacyEndpointPaths,
    runtime_dir: Option<&Path>,
    home: &Path,
) -> Result<(), String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command.arg("daemon").arg("stop");
    command.current_dir(home);
    command.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    command.env("HOME", home);
    command.env("TERM", "xterm-256color");
    command.env("TMPDIR", temp_dir);
    command.env(TEST_LEGACY_ENDPOINT_ROOT_ENV, &legacy.root);
    if let Some(runtime_dir) = runtime_dir {
        command.env("XDG_RUNTIME_DIR", runtime_dir);
    }
    command.env("DOT_AGENT_DECK_STATE_DIR", temp_dir.join("stop-state"));
    let output = command
        .output()
        .map_err(|error| format!("run fallback daemon stop: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "fallback daemon stop exited {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Scenario: Launch the real deck with isolated fallback and legacy roots, with `XDG_RUNTIME_DIR` and both endpoint overrides absent. The dashboard should render while the hook and attach sockets live inside a mode-0700 per-uid directory; a second launch with `XDG_RUNTIME_DIR` set should retain its established endpoint spellings, and neither launch should change the literal production legacy paths.
#[spec("error/socket/009")]
#[test]
fn socket_009_fallback_endpoints_use_owner_only_uid_directory() {
    let literal_legacy = LiteralLegacyEndpoints::capture();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let legacy_temp = common::harness_tempdir().expect("create isolated legacy endpoint root");
    let legacy = LegacyEndpointPaths::under(legacy_temp.path());
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("daemon.log");

    let deck = launch_fallback_deck(temp.path(), &legacy, &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths, &legacy, true);
    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(deck);

    let runtime = common::harness_tempdir().expect("create isolated XDG_RUNTIME_DIR");
    let xdg_hook = runtime.path().join("dot-agent-deck.sock");
    let xdg_attach = runtime.path().join("dot-agent-deck-attach.sock");
    let xdg_log = runtime.path().join("daemon.log");
    let xdg_deck = TuiDeck::builder()
        .without_endpoint_overrides()
        .with_env("TMPDIR", temp.path().to_string_lossy())
        .with_env(TEST_LEGACY_ENDPOINT_ROOT_ENV, legacy.root.to_string_lossy())
        .with_env("XDG_RUNTIME_DIR", runtime.path().to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", xdg_log.to_string_lossy())
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30")
        .launch_with_fixture("minimal");
    xdg_deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    for endpoint in [&xdg_attach, &xdg_hook] {
        match fs::symlink_metadata(endpoint) {
            Ok(metadata) if metadata.file_type().is_socket() => {}
            Ok(_) => failures.push(format!(
                "XDG endpoint {} exists but is not a Unix socket",
                endpoint.display()
            )),
            Err(error) => failures.push(format!(
                "established XDG endpoint {} is missing: {error}",
                endpoint.display()
            )),
        }
    }
    let xdg_home = xdg_deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, Some(runtime.path()), &xdg_home)
    {
        failures.push(error);
    }
    drop(xdg_deck);

    literal_legacy.assert_unchanged();
    assert!(
        failures.is_empty(),
        "fallback endpoints did not use the owner-only per-uid directory:\n{}",
        failures.join("\n")
    );
}

/// Scenario: Plant a regular file at an isolated legacy attach path, then launch the real deck with isolated fallback and legacy roots and endpoint overrides absent. Startup should still reach the dashboard, bind both endpoints in the new per-uid directory, leave the planted file untouched, and never change the literal production legacy paths.
#[spec("error/socket/010")]
#[test]
fn socket_010_legacy_path_squatter_does_not_wedge_startup() {
    let literal_legacy = LiteralLegacyEndpoints::capture();
    let legacy_temp = common::harness_tempdir().expect("create isolated legacy endpoint root");
    let legacy = LegacyEndpointPaths::under(legacy_temp.path());
    fs::write(&legacy.attach, LEGACY_SQUATTER_MARKER).expect("plant legacy attach-path file");
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("daemon.log");

    let deck = launch_fallback_deck(temp.path(), &legacy, &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths, &legacy, false);
    match fs::read_to_string(&legacy.attach) {
        Ok(contents) if contents == LEGACY_SQUATTER_MARKER => {}
        Ok(contents) => failures.push(format!(
            "legacy squatter file changed from {LEGACY_SQUATTER_MARKER:?} to {contents:?}"
        )),
        Err(error) => failures.push(format!(
            "legacy squatter file {} was removed or became unreadable: {error}",
            legacy.attach.display()
        )),
    }
    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(deck);

    literal_legacy.assert_unchanged();
    assert!(
        failures.is_empty(),
        "legacy-path squatter still affected fallback startup:\n{}",
        failures.join("\n")
    );
}

/// Scenario: Start the real daemon on isolated legacy hook and attach paths, then launch a fallback client with an isolated `TMPDIR` and endpoint overrides absent. The client should render through that daemon without a second lazy-spawn; after it exits, a fresh fallback launch should still bind the new primary pair, and the literal production legacy paths should remain unchanged.
#[spec("error/socket/011")]
#[test]
fn socket_011_fallback_client_discovers_legacy_daemon_without_lazy_spawn() {
    let literal_legacy = LiteralLegacyEndpoints::capture();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let legacy_temp = common::harness_tempdir().expect("create isolated legacy endpoint root");
    let legacy = LegacyEndpointPaths::under(legacy_temp.path());
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("shared-daemon.log");
    let hook = legacy.hook.to_string_lossy().into_owned();
    let attach = legacy.attach.to_string_lossy().into_owned();
    let legacy_root = legacy.root.to_string_lossy().into_owned();
    let log_value = log.to_string_lossy().into_owned();
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_SOCKET", hook.as_str()),
            ("DOT_AGENT_DECK_ATTACH_SOCKET", attach.as_str()),
            (TEST_LEGACY_ENDPOINT_ROOT_ENV, legacy_root.as_str()),
            ("DOT_AGENT_DECK_LOG", log_value.as_str()),
        ],
    );
    common::wait_for_file_contains(&log, "Attach protocol listening");

    let deck = launch_fallback_deck(temp.path(), &legacy, &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    let records = common::agent_records_on(&legacy.attach);
    assert!(
        records.is_empty(),
        "fresh legacy daemon had records: {records:?}"
    );
    assert!(
        !paths.attach.exists() && !paths.hook.exists(),
        "fallback client lazy-spawned a second daemon at {} / {}",
        paths.attach.display(),
        paths.hook.display()
    );
    let log_contents = fs::read_to_string(&log).expect("read shared daemon log");
    let listener_count = log_contents.matches("Attach protocol listening").count();
    assert_eq!(
        listener_count, 1,
        "expected one daemon listener, found {listener_count}:\n{log_contents}"
    );

    drop(deck);
    drop(daemon);

    let primary_log = temp.path().join("primary-daemon.log");
    let primary = launch_fallback_deck(temp.path(), &legacy, &primary_log);
    primary.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths, &legacy, false);
    let home = primary.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(primary);
    literal_legacy.assert_unchanged();
    assert!(
        failures.is_empty(),
        "fresh fallback launch after the legacy attach did not use the new primary endpoints:\n{}",
        failures.join("\n")
    );
}
