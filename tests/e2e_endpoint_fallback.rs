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

fn inspect_new_endpoints(paths: &EndpointPaths) -> Vec<String> {
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
    failures
}

/// Issue #1211: is `path` an owner-only Unix socket — the shape the daemon's
/// legacy alias must have for an older client to trust it at all.
fn is_owner_only_socket(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|md| md.file_type().is_socket() && md.mode() & 0o777 == 0o600)
        .unwrap_or(false)
}

/// Issue #1211: wait for the daemon's exit to have unlinked `path`. `daemon
/// stop` reports once the daemon is gone, and the alias is released just
/// before that, so this normally holds on the first check; the bound is for a
/// loaded box.
fn wait_until_absent(path: &Path) -> bool {
    common::wait_until(std::time::Duration::from_secs(10), || {
        fs::symlink_metadata(path).is_err()
    })
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

/// Scenario: Launch the real deck with isolated fallback and legacy roots, with `XDG_RUNTIME_DIR` and both endpoint overrides absent. The dashboard should render while the hook and attach sockets live inside a mode-0700 per-uid directory, and the same daemon should also answer at the redirected pre-#1121 hook and attach spellings so an older client finds it, removing both when it stops; a second launch with `XDG_RUNTIME_DIR` set should retain its established endpoint spellings and bind no alias, and neither launch should change the literal production legacy paths.
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
    let mut failures = inspect_new_endpoints(&paths);

    // Issue #1211: the old spellings are aliases of THIS daemon, not a second
    // one. Both are owner-only sockets; the attach alias answers a real
    // `ListAgents`; and the log still holds exactly one `Attach protocol
    // listening` line, the count an operator (and `cargo xver`) reads as "one
    // daemon".
    for alias in [&legacy.hook, &legacy.attach] {
        if !is_owner_only_socket(alias) {
            failures.push(format!(
                "the pre-#1121 alias {} is not an owner-only socket while the fallback daemon runs",
                alias.display()
            ));
        }
    }
    let records = common::agent_records_on(&legacy.attach);
    if !records.is_empty() {
        failures.push(format!(
            "the legacy attach alias reached a daemon with records: {records:?}"
        ));
    }
    let log_contents = fs::read_to_string(&log).unwrap_or_default();
    let listeners = log_contents.matches("Attach protocol listening").count();
    if listeners != 1 {
        failures.push(format!(
            "expected one daemon behind the primary and its aliases, found {listeners} \
             `Attach protocol listening` lines:\n{log_contents}"
        ));
    }

    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(deck);
    for alias in [&legacy.hook, &legacy.attach] {
        if !wait_until_absent(alias) {
            failures.push(format!(
                "the pre-#1121 alias {} was left behind after the daemon stopped",
                alias.display()
            ));
        }
    }

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
    // Issue #1211: the XDG spellings are identical in every build, so there is
    // no older client to serve and no alias to bind.
    for alias in [&legacy.hook, &legacy.attach] {
        if fs::symlink_metadata(alias).is_ok() {
            failures.push(format!(
                "an XDG_RUNTIME_DIR daemon bound the pre-#1121 alias {}",
                alias.display()
            ));
        }
    }
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

/// Scenario: Plant a regular file at an isolated legacy attach path, then launch the real deck with isolated fallback and legacy roots and endpoint overrides absent. Startup should still reach the dashboard, bind both endpoints in the new per-uid directory and the unsquatted legacy hook alias, leave the planted file untouched both while the daemon runs and after it stops, and never change the literal production legacy paths.
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

    let check_squatter =
        |failures: &mut Vec<String>, when: &str| {
            match fs::read_to_string(
        &legacy.attach,
    ) {
        Ok(contents) if contents == LEGACY_SQUATTER_MARKER => {}
        Ok(contents) => failures.push(format!(
            "{when}: legacy squatter file changed from {LEGACY_SQUATTER_MARKER:?} to {contents:?}"
        )),
        Err(error) => failures.push(format!(
            "{when}: legacy squatter file {} was removed or became unreadable: {error}",
            legacy.attach.display()
        )),
    }
        };

    let deck = launch_fallback_deck(temp.path(), &legacy, &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);
    let mut failures = inspect_new_endpoints(&paths);
    check_squatter(&mut failures, "while the daemon runs");
    // Issue #1211: the squatter costs the attach alias and nothing else — the
    // daemon started (the dashboard above), its primary pair is bound, and the
    // hook alias beside the squatted one still binds.
    if !is_owner_only_socket(&legacy.hook) {
        failures.push(format!(
            "the unsquatted legacy hook alias {} was not bound beside the squatted attach path",
            legacy.hook.display()
        ));
    }
    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(deck);
    // …and the daemon's exit removes its own alias and never the squatter.
    if !wait_until_absent(&legacy.hook) {
        failures.push(format!(
            "the legacy hook alias {} was left behind after the daemon stopped",
            legacy.hook.display()
        ));
    }
    check_squatter(&mut failures, "after the daemon stopped");

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
    let mut failures = inspect_new_endpoints(&paths);
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

/// A listener bound at the legacy attach path that accepts a connection and
/// immediately drops it, without ever speaking the attach protocol.
///
/// This is the *client-visible* shape of a legacy daemon that goes away
/// between the launcher's answering-probe and the handshake's own `connect`:
/// `verify_endpoint_trusted` passes (uid-equal, exactly `0o600`, a socket),
/// `IpcClient::connect_timeout` succeeds, so the resolver selects the legacy
/// endpoint — and then the `Hello` round-trip gets EOF instead of a reply.
/// Reproducing it this way rather than by racing a real daemon's exit makes
/// the window deterministic; the window itself is two syscalls wide and
/// nothing in the code can be asked to sit inside it.
struct HalfDeadLegacyDaemon {
    path: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HalfDeadLegacyDaemon {
    fn bind(path: &Path) -> Self {
        let listener =
            std::os::unix::net::UnixListener::bind(path).expect("bind the half-dead legacy daemon");
        // Exactly `0o600` or `verify_endpoint_trusted` refuses the endpoint
        // and the resolver never selects it — which would make this test pass
        // by never reaching the branch it exists to cover.
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("chmod the half-dead legacy endpoint to 0o600");
        // A BLOCKING accept loop, woken by `shutdown`'s own connect. A
        // non-blocking listener polled on a timer would be the obvious
        // spelling and is exactly what Decision 21 forbids — and rightly:
        // the poll interval would be load-bearing on a busy box, while
        // blocking here costs nothing and ends deterministically.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = std::sync::Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while let Ok(_conn) = listener.accept() {
                // `_conn` is dropped right here: the peer sees the connect
                // succeed and its first read return EOF, which is the whole
                // behaviour being staged.
                if stop_for_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
            }
        });
        Self {
            path: path.to_path_buf(),
            stop,
            thread: Some(thread),
        }
    }

    /// Stop answering and remove the inode, so the `daemon stop` the test runs
    /// afterwards resolves to the primary endpoint rather than to this stub.
    ///
    /// The flag is set *before* the wake-up connect, and the connect is what
    /// unblocks `accept`; ordering it the other way round would race. The
    /// connect is allowed to fail — if the thread has already exited for its
    /// own reasons there is nothing to wake, and the `join` below still ends.
    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        let _ = thread.join();
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for HalfDeadLegacyDaemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Scenario: Bind a stub at the isolated legacy attach path that accepts connections but never answers the attach protocol, then launch the real deck with isolated fallback and legacy roots and both endpoint overrides absent. The launcher should select that legacy endpoint, get a failed handshake probe from it, and recover by cold-starting at the new per-uid endpoints instead of exiting — so the dashboard still renders and both new sockets are bound.
#[spec("error/socket/012")]
#[test]
fn socket_012_a_legacy_daemon_that_stops_answering_recovers_to_the_primary() {
    let literal_legacy = LiteralLegacyEndpoints::capture();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let legacy_temp = common::harness_tempdir().expect("create isolated legacy endpoint root");
    let legacy = LegacyEndpointPaths::under(legacy_temp.path());
    let paths = EndpointPaths::under(temp.path());
    let log = temp.path().join("daemon.log");

    let mut half_dead = HalfDeadLegacyDaemon::bind(&legacy.attach);

    // The whole assertion: without the recovery this launch prints
    // `build-version handshake probe failed: …` and exits `FAILURE`, so the
    // dashboard never appears and this wait is what reddens.
    let deck = launch_fallback_deck(temp.path(), &legacy, &log);
    deck.wait_for_string(DASHBOARD_EMPTY_STATE);

    // …and it recovered to the PRIMARY endpoint rather than limping along on
    // the legacy one: the new per-uid pair is bound, which only the cold start
    // at `primary_attach_endpoint()` can have done.
    let mut failures = inspect_new_endpoints(&paths);

    half_dead.shutdown();
    let home = deck.home_dir().to_path_buf();
    if let Err(error) = stop_resolved_daemon(temp.path(), &legacy, None, &home) {
        failures.push(error);
    }
    drop(deck);

    literal_legacy.assert_unchanged();
    assert!(
        failures.is_empty(),
        "a legacy endpoint that stopped answering did not recover to the primary:\n{}",
        failures.join("\n")
    );
}

/// A directory some other uid owns, to bind over the per-uid fallback name so
/// the deck under test sees it as squatted, and confirmation that `bwrap` can
/// do that on this host — or `None`, after printing `SKIP:`.
///
/// Issue #1173 is about an entry **another uid** owns, and a test has no second
/// uid to create one with. A bubblewrap mount namespace supplies the ownership
/// without one: bind a root-owned directory (read-only) over an empty
/// directory we made at the per-uid name, and inside the namespace `lstat`
/// reports the root-owned inode — as `0`, or as the overflow uid when bwrap
/// runs in a user namespace. Either is not ours, which is all the rule reads.
/// Where bwrap is absent or cannot make a namespace (a CI runner with
/// unprivileged user namespaces restricted), nothing here can stand the
/// scenario up, so the test skips rather than passes on a vacuous run.
fn bwrap_squat_source(scratch: &Path) -> Option<PathBuf> {
    let source = PathBuf::from("/usr");
    let owner = fs::symlink_metadata(&source).map(|m| m.uid()).ok();
    if owner.is_none() || owner == Some(current_uid()) {
        eprintln!(
            "SKIP: no directory another uid owns to squat with (running as the owner of /usr?)"
        );
        return None;
    }
    let mountpoint = scratch.join("probe");
    fs::create_dir(&mountpoint).expect("create the bwrap probe mountpoint");
    let probe = Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "--ro-bind"])
        .arg(&source)
        .arg(&mountpoint)
        .args(["--", "/bin/sh", "-c", "stat -c %u \"$0\""])
        .arg(&mountpoint)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output();
    match probe {
        Ok(output) if output.status.success() => {
            let seen = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if seen == current_uid().to_string() {
                eprintln!("SKIP: bwrap's bind did not present a foreign owner (saw uid {seen})");
                return None;
            }
            Some(source)
        }
        Ok(output) => {
            eprintln!(
                "SKIP: bwrap cannot create the mount namespace this scenario needs: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            None
        }
        Err(error) => {
            eprintln!("SKIP: bwrap is not available: {error}");
            None
        }
    }
}

/// What [`socket_014_a_foreign_owned_uid_directory_relocates_instead_of_wedging`]
/// runs inside the namespace. A headless `daemon serve`, then the clients that
/// must find it, then a stop by its own pid; then the launcher's lazy-spawn
/// path, which must land on the same relocated directory, stopped with
/// `daemon stop`. Each step's output goes to a file under `$OUT` for the test
/// to read after the namespace — and every process in it — is gone.
const SQUAT_SCENARIO: &str = r#"
set -u
legacy_attach="$DOT_AGENT_DECK_TEST_LEGACY_ENDPOINT_ROOT/dot-agent-deck-attach-$(id -u).sock"
"$DAD_BIN" daemon serve >"$OUT/serve.out" 2>&1 &
pid=$!
i=0
until "$DAD_BIN" daemon endpoint >"$OUT/endpoint" 2>"$OUT/endpoint.err"; do
    i=$((i+1)); [ "$i" -ge 150 ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
done
"$DAD_BIN" daemon status >"$OUT/status" 2>&1; echo $? >"$OUT/status.rc"
[ -S "$legacy_attach" ] && echo bound >"$OUT/alias"
kill "$pid"; wait "$pid"
DOT_AGENT_DECK_EXIT_AFTER_HANDSHAKE=1 "$DAD_BIN" </dev/null >"$OUT/launch" 2>&1; echo $? >"$OUT/launch.rc"
"$DAD_BIN" daemon endpoint >"$OUT/endpoint2" 2>&1
"$DAD_BIN" daemon stop >"$OUT/stop" 2>&1; echo $? >"$OUT/stop.rc"
"#;

/// Scenario: Inside a bubblewrap namespace, bind a root-owned directory over the isolated `TMPDIR`'s `dot-agent-deck-<uid>` name so it reads as another user's, then run the real `daemon serve` with `XDG_RUNTIME_DIR` and both endpoint overrides absent. Instead of refusing to start, the daemon should bind inside one new owner-only `dot-agent-deck-<uid>.<16 hex>` directory that `daemon endpoint` and `daemon status` both find, still holding the redirected legacy alias; a later lazy-spawn through the launcher should reuse that same directory, and the literal production legacy paths should stay unchanged.
#[spec("error/socket/014")]
#[test]
fn socket_014_a_foreign_owned_uid_directory_relocates_instead_of_wedging() {
    let scratch = common::harness_tempdir().expect("create the scenario's scratch root");
    let Some(squat_source) = bwrap_squat_source(scratch.path()) else {
        return;
    };
    let literal_legacy = LiteralLegacyEndpoints::capture();
    let temp = common::harness_tempdir().expect("create fallback TMPDIR");
    let legacy_temp = common::harness_tempdir().expect("create isolated legacy endpoint root");
    let legacy = LegacyEndpointPaths::under(legacy_temp.path());
    let paths = EndpointPaths::under(temp.path());
    fs::create_dir(&paths.dir).expect("create the mountpoint the squat is bound over");
    let out = scratch.path().join("out");
    let log = scratch.path().join("deck.log");
    for dir in [
        &out,
        &scratch.path().join("home"),
        &scratch.path().join("locks"),
    ] {
        fs::create_dir(dir).expect("create a scenario directory");
    }

    let output = Command::new("bwrap")
        .args(["--dev-bind", "/", "/", "--ro-bind"])
        .arg(&squat_source)
        .arg(&paths.dir)
        .args([
            "--unshare-pid",
            "--die-with-parent",
            "--",
            "/bin/sh",
            "-c",
            SQUAT_SCENARIO,
        ])
        .current_dir(scratch.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TERM", "xterm-256color")
        .env("HOME", scratch.path().join("home"))
        .env("TMPDIR", temp.path())
        .env(TEST_LEGACY_ENDPOINT_ROOT_ENV, &legacy.root)
        .env("DOT_AGENT_DECK_STATE_DIR", scratch.path().join("state"))
        .env("DOT_AGENT_DECK_LOCK_DIR", scratch.path().join("locks"))
        .env("DOT_AGENT_DECK_LOG", &log)
        .env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0")
        .env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "60")
        .env("DOT_AGENT_DECK_EXPERIMENTAL", "0")
        .env("DAD_BIN", env!("CARGO_BIN_EXE_dot-agent-deck"))
        .env("OUT", &out)
        .output()
        .expect("run the squat scenario under bwrap");
    let read = |name: &str| fs::read_to_string(out.join(name)).unwrap_or_default();
    let context = format!(
        "bwrap exited {:?}\nstderr:\n{}\nserve:\n{}\nlaunch:\n{}\nlog:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        read("serve.out"),
        read("launch"),
        fs::read_to_string(&log).unwrap_or_default()
    );

    let mut failures = Vec::new();
    let uid = current_uid();
    let prefix = format!("dot-agent-deck-{uid}.");
    let relocated: Vec<PathBuf> = fs::read_dir(temp.path())
        .expect("list the fallback TMPDIR")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .map(|entry| entry.path())
        .collect();
    match relocated.as_slice() {
        [dir] => {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let digits = &name[prefix.len()..];
            if digits.len() != 16 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                failures.push(format!(
                    "relocated directory name {name} is not <prefix><16 hex>"
                ));
            }
            let metadata = fs::symlink_metadata(dir).expect("stat the relocated directory");
            if !metadata.file_type().is_dir()
                || metadata.uid() != uid
                || metadata.permissions().mode() & 0o777 != 0o700
            {
                failures.push(format!(
                    "relocated directory {} is not an owner-only directory of ours: {metadata:?}",
                    dir.display()
                ));
            }
            let expected = dir.join("attach.sock");
            for (step, file) in [("daemon serve", "endpoint"), ("lazy-spawn", "endpoint2")] {
                let reported = read(file);
                if reported.trim() != expected.to_string_lossy() {
                    failures.push(format!(
                        "after {step}, `daemon endpoint` answered {reported:?}, not the relocated {} \
                         (stderr: {})",
                        expected.display(),
                        read("endpoint.err")
                    ));
                }
            }
            let log_contents = fs::read_to_string(&log).unwrap_or_default();
            let listening: Vec<&str> = log_contents
                .lines()
                .filter(|line| line.contains("Attach protocol listening"))
                .collect();
            if listening.len() != 2
                || !listening
                    .iter()
                    .all(|line| line.contains(&*expected.to_string_lossy()))
            {
                failures.push(format!(
                    "expected exactly two daemons, both listening on {}, found: {listening:?}",
                    expected.display()
                ));
            }
            if !log_contents.contains("another user owns the usual directory") {
                failures.push("the daemon did not log why it relocated".to_string());
            }
        }
        other => failures.push(format!(
            "expected exactly one relocated directory in {}, found {other:?}",
            temp.path().display()
        )),
    }
    if read("status.rc").trim() != "0" || !read("status").contains("no managed agents") {
        failures.push(format!(
            "`daemon status` did not reach the relocated daemon: rc {:?}, output {:?}",
            read("status.rc"),
            read("status")
        ));
    }
    if read("alias").trim() != "bound" {
        failures.push(format!(
            "the relocated daemon did not bind the redirected legacy attach alias {}",
            legacy.attach.display()
        ));
    }
    if read("launch.rc").trim() != "0" {
        failures.push(format!(
            "the launcher's lazy-spawn did not complete: rc {:?}",
            read("launch.rc")
        ));
    }
    if read("stop.rc").trim() != "0" {
        failures.push(format!(
            "`daemon stop` did not reach the lazy-spawned relocated daemon: rc {:?}, output {:?}",
            read("stop.rc"),
            read("stop")
        ));
    }
    match fs::read_dir(&paths.dir).map(|entries| entries.count()) {
        Ok(0) => {}
        other => failures.push(format!(
            "the per-uid mountpoint {} gained entries or vanished: {other:?}",
            paths.dir.display()
        )),
    }

    literal_legacy.assert_unchanged();
    assert!(
        failures.is_empty(),
        "a foreign-owned per-uid directory still wedged or misrouted startup:\n{}\n\n{context}",
        failures.join("\n")
    );
}
