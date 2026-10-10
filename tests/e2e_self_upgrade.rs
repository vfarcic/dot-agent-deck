#![cfg(all(feature = "e2e", unix))]

//! Upgrading this machine's copy of dot-agent-deck (issue #1635), through the
//! real binary against a fake release server: the TUI's badge, its upgrade
//! dialog and a confirmed upgrade, the periodic re-check, and the `upgrade`
//! subcommand.
//!
//! The binary under test is told, through `e2e`-only seams, that it runs an
//! older version (`DOT_AGENT_DECK_TEST_RUNNING_VERSION`) from a path the test
//! chose (`DOT_AGENT_DECK_TEST_RUNNING_EXE`), and where releases are looked up
//! and downloaded (`DOT_AGENT_DECK_TEST_RELEASES_API_URL`,
//! `DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL`,
//! `DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE`). The "release binary" it
//! downloads is a small script that answers `--version` as dot-agent-deck
//! [`RELEASE`]: the core checks a download only by its checksum and that
//! answer, so the script exercises the whole download → check → replace path
//! without moving a few hundred megabytes of debug binary per test.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::self_upgrade::Platform;
use spec::spec;

/// What a running copy older than the release reports.
const OLD_VERSION: &str = "0.0.1";
/// The release the fake server offers.
const RELEASE: &str = "9.9.9";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

fn cli_asset() -> &'static str {
    Platform::current()
        .expect("a platform with release assets")
        .cli_asset()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A local stand-in for GitHub's release API and download host.
struct FakeReleases {
    port: u16,
    latest: Arc<Mutex<String>>,
}

impl FakeReleases {
    /// Serve `latest` as the newest stable release, its CLI asset as `asset`,
    /// and `manifest` as its `checksums.txt`.
    fn start(latest: &str, asset: Vec<u8>, manifest: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake release server");
        let port = listener.local_addr().unwrap().port();
        let latest = Arc::new(Mutex::new(latest.to_string()));
        let shared = latest.clone();
        let asset = Arc::new(asset);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let latest = shared.lock().unwrap().clone();
                let asset = asset.clone();
                let manifest = manifest.clone();
                std::thread::spawn(move || serve(stream, &latest, &asset, &manifest));
            }
        });
        Self { port, latest }
    }

    fn set_latest(&self, version: &str) {
        *self.latest.lock().unwrap() = version.to_string();
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The seams that point the binary under test at this server, as a copy
    /// running `OLD_VERSION` from `exe`.
    fn env(&self, exe: &Path) -> Vec<(String, String)> {
        vec![
            (
                "DOT_AGENT_DECK_TEST_RELEASES_API_URL".into(),
                format!("{}/api/latest", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL".into(),
                format!("{}/api/list", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE".into(),
                format!("{}/download", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RUNNING_VERSION".into(),
                OLD_VERSION.into(),
            ),
            (
                "DOT_AGENT_DECK_TEST_RUNNING_EXE".into(),
                exe.to_str().expect("UTF-8 path").into(),
            ),
        ]
    }
}

fn serve(mut stream: TcpStream, latest: &str, asset: &[u8], manifest: &str) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let release = format!(r#"{{"tag_name":"v{latest}","draft":false,"prerelease":false}}"#);
    let download = format!("/download/v{latest}/");
    let (status, body): (&str, Vec<u8>) = if path == "/api/latest" {
        ("200 OK", release.into_bytes())
    } else if path.starts_with("/api/list") {
        ("200 OK", format!("[{release}]").into_bytes())
    } else if let Some(name) = path.strip_prefix(&download) {
        if name == "checksums.txt" {
            ("200 OK", manifest.as_bytes().to_vec())
        } else if name == cli_asset() {
            ("200 OK", asset.to_vec())
        } else {
            ("404 Not Found", Vec::new())
        }
    } else {
        ("404 Not Found", Vec::new())
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
}

/// The release binary the fake server offers — a script that answers
/// `--version` as dot-agent-deck [`RELEASE`] — and a checksum manifest that
/// lists it (or a wrong checksum).
fn release(correct_checksum: bool) -> (Vec<u8>, String) {
    let asset = format!("#!/bin/sh\necho 'dot-agent-deck {RELEASE}'\n").into_bytes();
    let sha = if correct_checksum {
        sha256_hex(&asset)
    } else {
        "0".repeat(64)
    };
    let manifest = format!("{sha}  {}\n", cli_asset());
    (asset, manifest)
}

/// A "downloaded binary" in a folder the user can write: the file the
/// upgrade replaces.
fn writable_install(dir: &Path) -> PathBuf {
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("dot-agent-deck");
    std::fs::write(&exe, b"#!/bin/sh\necho old\n").unwrap();
    exe
}

fn wait_for_file(path: &Path, want: &[u8], timeout: Duration) -> bool {
    common::wait_until(timeout, || {
        std::fs::read(path).is_ok_and(|bytes| bytes == want)
    })
}

fn deck(server: &FakeReleases, exe: &Path, extra: &[(&str, &str)]) -> TuiDeck {
    let mut builder = TuiDeck::builder().with_pty_size(200, 50);
    for (key, value) in server.env(exe) {
        builder = builder.with_env(key, value);
    }
    for (key, value) in extra {
        builder = builder.with_env(*key, *value);
    }
    builder.launch_with_fixture("minimal")
}

/// Run `dot-agent-deck upgrade <args>` against `server`, as a copy at `exe`.
fn run_upgrade(server: &FakeReleases, exe: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(bin());
    command.arg("upgrade").args(args).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("DOT_AGENT_DECK_STATE_DIR", home.join("state"))
        .env("DOT_AGENT_DECK_SOCKET", home.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", home.join("attach.sock"));
    for (key, value) in server.env(exe) {
        command.env(key, value);
    }
    command.output().expect("run dot-agent-deck upgrade")
}

/// Scenario: Start the TUI as an old copy installed in a writable folder, with a fake release server offering a newer release. The footer badge names the release and the key; pressing `u` opens the dialog with the core's in-place plan, which says provenance will not be checked; choosing Upgrade replaces the file with the served release and the dialog shows the result and says to restart the TUI.
#[spec("upgrade/tui-upgrade/001")]
#[test]
fn tui_upgrade_001_badge_key_confirm_replaces_the_binary() {
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(version, asset.clone(), manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());
    let old = std::fs::read(&exe).unwrap();

    let deck = deck(&server, &exe, &[]);
    deck.wait_for_string(&format!(
        "update available: v{version} (current: v{OLD_VERSION})"
    ));
    deck.wait_for_string("u to upgrade");

    deck.send_keys(b"u");
    deck.wait_for_string(&format!("Upgrade to v{version}"));
    deck.wait_for_string("Downloaded binary at");
    deck.wait_for_string("Build provenance will NOT be checked");
    deck.wait_for_string(&format!("Upgrade dot-agent-deck to v{version}?"));
    deck.wait_for_string("> Cancel");
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        old,
        "nothing changes before the user confirms"
    );

    deck.send_keys(b"\x1b[B");
    deck.wait_for_string("> Upgrade");
    deck.send_keys(b"\r");
    assert!(
        wait_for_file(&exe, &asset, Duration::from_secs(60)),
        "the confirmed upgrade must replace {} with the served release",
        exe.display()
    );
    deck.wait_for_string("Upgraded ");
    deck.wait_for_string("Build provenance was not checked");
    deck.wait_for_string("start it again");

    deck.send_keys(b"\x1b");
    deck.wait_for_absence(&format!("Upgrade to v{version}"));
}

/// Scenario: Start the TUI as an old copy installed with Nix, with a fake release server offering a newer release, and press `u`. The dialog says the copy is not changed from here and what to run instead, offers only Close and no confirmation, and Enter closes it.
#[spec("upgrade/tui-upgrade/002")]
#[test]
fn tui_upgrade_002_nix_copy_is_notify_only() {
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(version, asset, manifest);
    let exe = PathBuf::from("/nix/store/0000000000000000-dot-agent-deck-0.0.1/bin/dot-agent-deck");

    let deck = deck(&server, &exe, &[]);
    deck.wait_for_string(&format!("update available: v{version}"));
    deck.send_keys(b"u");
    deck.wait_for_string("Installed with Nix");
    deck.wait_for_string("> Close");
    let screen = deck.snapshot_grid();
    assert!(
        !screen.contains("Upgrade dot-agent-deck to"),
        "a Nix copy is never offered an upgrade\n{screen}"
    );
    assert!(!screen.contains("Cancel"), "{screen}");

    deck.send_keys(b"\r");
    deck.wait_for_absence("Installed with Nix");
}

/// Scenario: Start the TUI with a re-check interval of one second and a fake release server whose newest release is the version the TUI runs, so no badge shows; then publish a newer release on the server. The badge appears without restarting the TUI.
#[spec("upgrade/tui-upgrade/003")]
#[test]
fn tui_upgrade_003_periodic_recheck_notices_a_new_release() {
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(OLD_VERSION, asset, manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());

    let deck = deck(
        &server,
        &exe,
        &[("DOT_AGENT_DECK_TEST_UPDATE_RECHECK_SECS", "1")],
    );
    deck.wait_for_string("No active agents");
    // Several re-checks run in these three seconds; none may show a badge.
    let badge_shown = common::wait_until(Duration::from_secs(3), || {
        deck.snapshot_grid().contains("update available")
    });
    assert!(!badge_shown, "{}", deck.snapshot_grid());

    server.set_latest(version);
    assert!(
        deck.wait_for_grid_string_within(
            &format!("update available: v{version}"),
            Duration::from_secs(30)
        ),
        "the TUI must notice a release published while it runs\n{}",
        deck.snapshot_grid()
    );
}

/// Scenario: Run `dot-agent-deck upgrade --check` as an old copy in a writable folder against a fake release server. It prints the plan — the headline, the in-place replacement and that provenance will not be checked — and changes nothing.
#[spec("upgrade/cli-upgrade/001")]
#[test]
fn cli_upgrade_001_check_prints_the_plan_and_changes_nothing() {
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(version, asset, manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());
    let old = std::fs::read(&exe).unwrap();

    let out = run_upgrade(&server, &exe, dir.path(), &["--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "dot-agent-deck: update available: v{version} (current: v{OLD_VERSION})"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("Downloaded binary at {}.", exe.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains("Build provenance will NOT be checked"),
        "{stdout}"
    );
    assert_eq!(std::fs::read(&exe).unwrap(), old, "--check changes nothing");
}

/// Scenario: Run `dot-agent-deck upgrade --yes` as an old copy in a writable folder against a fake release server. It downloads the release, checks it against the checksum manifest, replaces the file and says so.
#[spec("upgrade/cli-upgrade/002")]
#[test]
fn cli_upgrade_002_yes_replaces_the_binary() {
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(version, asset.clone(), manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());

    let out = run_upgrade(&server, &exe, dir.path(), &["--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("Upgraded {} to v{version}.", exe.display())),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        asset,
        "the file is the served release"
    );
}

/// Scenario: Run `dot-agent-deck upgrade --yes` against a fake release server whose checksum manifest does not match the download. The upgrade aborts with the core's mismatch message, exits non-zero, and leaves the old file in place.
#[spec("upgrade/cli-upgrade/003")]
#[test]
fn cli_upgrade_003_checksum_mismatch_leaves_the_old_binary() {
    let version = RELEASE;
    let (asset, manifest) = release(false);
    let server = FakeReleases::start(version, asset, manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());
    let old = std::fs::read(&exe).unwrap();

    let out = run_upgrade(&server, &exe, dir.path(), &["--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "a mismatch must fail\n{stdout}");
    assert!(
        stdout.contains(&format!("{} does not match checksums.txt", cli_asset())),
        "{stdout}"
    );
    assert!(stdout.contains("Nothing was changed."), "{stdout}");
    assert_eq!(std::fs::read(&exe).unwrap(), old, "the old binary stays");
}
