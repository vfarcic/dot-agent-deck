#![cfg(all(feature = "e2e", unix))]

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TuiDeck;
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

struct LegacyEndpointClaim {
    hook: PathBuf,
    attach: PathBuf,
}

impl LegacyEndpointClaim {
    fn take() -> Self {
        let uid = current_uid();
        let claim = Self {
            hook: PathBuf::from(format!("/tmp/dot-agent-deck-{uid}.sock")),
            attach: PathBuf::from(format!("/tmp/dot-agent-deck-attach-{uid}.sock")),
        };
        for endpoint in [&claim.hook, &claim.attach] {
            assert!(
                matches!(
                    fs::symlink_metadata(endpoint),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ),
                "legacy endpoint {} must be absent before this isolated scenario starts",
                endpoint.display()
            );
        }
        claim
    }

    fn clear_entries(&self) -> Result<(), String> {
        for endpoint in [&self.hook, &self.attach] {
            match fs::remove_file(endpoint) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "remove claimed legacy endpoint {}: {error}",
                        endpoint.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Drop for LegacyEndpointClaim {
    fn drop(&mut self) {
        if let Err(error) = self.clear_entries() {
            eprintln!("[endpoint-fallback-test] {error}");
        }
    }
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments and simply returns the caller's
    // effective uid.
    unsafe { libc::geteuid() }
}

fn launch_fallback_deck(temp_dir: &Path, log_path: &Path) -> TuiDeck {
    TuiDeck::builder()
        .without_endpoint_overrides()
        .with_env("TMPDIR", temp_dir.to_string_lossy())
        .with_env("DOT_AGENT_DECK_LOG", log_path.to_string_lossy())
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30")
        .launch_with_fixture("minimal")
}

fn inspect_new_endpoints(
    paths: &EndpointPaths,
    legacy: &LegacyEndpointClaim,
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

/// Scenario: Launch the real deck with an isolated `TMPDIR`, with `XDG_RUNTIME_DIR` and both endpoint overrides absent. The dashboard should render while the hook and attach sockets live inside a mode-0700 per-uid directory; a second launch with `XDG_RUNTIME_DIR` set should retain its established endpoint spellings.
#[spec("error/socket/009")]
#[test]
fn socket_009_fallback_endpoints_use_owner_only_uid_directory() {
    let legacy = LegacyEndpointClaim::take();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("daemon.log");

    let deck = launch_fallback_deck(temp.path(), &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths, &legacy, true);
    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), None, &home) {
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
    if let Err(error) = stop_resolved_daemon(temp.path(), Some(runtime.path()), &xdg_home) {
        failures.push(error);
    }
    drop(xdg_deck);

    assert!(
        failures.is_empty(),
        "fallback endpoints did not use the owner-only per-uid directory:\n{}",
        failures.join("\n")
    );
}

/// Scenario: Plant a regular file at the literal legacy attach path, then launch the real deck with an isolated fallback `TMPDIR` and endpoint overrides absent. Startup should still reach the dashboard, bind both endpoints in the new per-uid directory, and leave the planted legacy file untouched.
#[spec("error/socket/010")]
#[test]
fn socket_010_legacy_path_squatter_does_not_wedge_startup() {
    let legacy = LegacyEndpointClaim::take();
    fs::write(&legacy.attach, LEGACY_SQUATTER_MARKER).expect("plant legacy attach-path file");
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("daemon.log");

    let deck = launch_fallback_deck(temp.path(), &log);
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
    if let Err(error) = stop_resolved_daemon(temp.path(), None, &home) {
        failures.push(error);
    }
    drop(deck);

    assert!(
        failures.is_empty(),
        "legacy-path squatter still affected fallback startup:\n{}",
        failures.join("\n")
    );
}

/// Scenario: Start the real daemon on the literal legacy hook and attach paths, then launch a fallback client with an isolated `TMPDIR` and endpoint overrides absent. The client should render through that daemon without a second lazy-spawn; after it exits, a fresh fallback launch should still bind the new primary endpoint pair.
#[spec("error/socket/011")]
#[test]
fn socket_011_fallback_client_discovers_legacy_daemon_without_lazy_spawn() {
    let legacy = LegacyEndpointClaim::take();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("shared-daemon.log");
    let hook = legacy.hook.to_string_lossy().into_owned();
    let attach = legacy.attach.to_string_lossy().into_owned();
    let log_value = log.to_string_lossy().into_owned();
    let daemon = common::spawn_daemon_serve_with_env(
        None,
        "0",
        &[
            ("DOT_AGENT_DECK_SOCKET", hook.as_str()),
            ("DOT_AGENT_DECK_ATTACH_SOCKET", attach.as_str()),
            ("DOT_AGENT_DECK_LOG", log_value.as_str()),
        ],
    );
    common::wait_for_file_contains(&log, "Attach protocol listening");

    let deck = launch_fallback_deck(temp.path(), &log);
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
    legacy
        .clear_entries()
        .expect("remove stopped legacy daemon endpoints");

    let primary_log = temp.path().join("primary-daemon.log");
    let primary = launch_fallback_deck(temp.path(), &primary_log);
    primary.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths, &legacy, true);
    let home = primary.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), None, &home) {
        failures.push(error);
    }
    drop(primary);
    assert!(
        failures.is_empty(),
        "fresh fallback launch after the legacy attach did not use the new primary endpoints:\n{}",
        failures.join("\n")
    );
}
