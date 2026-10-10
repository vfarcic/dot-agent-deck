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
//! downloads is a small script that answers `--version`, and nothing else
//! ([`release_script`]).
//!
//! Every test runs isolated from the machine it runs on ([`sandbox_env`]): a
//! `PATH` holding only a stand-in `dpkg-query` that reports nothing installed,
//! and the system folders where the other copy is looked for
//! (`/Applications`, `/usr/bin`) re-rooted into the test's own folder through
//! the `e2e`-only `DOT_AGENT_DECK_TEST_SYSTEM_ROOT`. So a desktop app, a
//! `gh` or a `.deb` installed on the host cannot change what a test sees.

mod common;
#[path = "support/fake_releases.rs"]
mod fake_releases;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use common::TuiDeck;
use fake_releases::{FakeReleases, cli_asset, release_script, sha256_hex};
use spec::spec;

/// What a running copy older than the release reports.
const OLD_VERSION: &str = "0.0.1";
/// The release the fake server offers.
const RELEASE: &str = "9.9.9";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

/// The release binary the fake server offers — a script that answers
/// `--version` as dot-agent-deck [`RELEASE`] — and a checksum manifest that
/// lists it (or a wrong checksum).
fn release(correct_checksum: bool) -> (Vec<u8>, String) {
    release_reporting(RELEASE, correct_checksum)
}

/// As [`release`], but the script answers `--version` as `reported`.
fn release_reporting(reported: &str, correct_checksum: bool) -> (Vec<u8>, String) {
    let asset = release_script(reported);
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

/// The environment that keeps a test from seeing the host's installs: `PATH`
/// is one folder under `dir` holding only a `dpkg-query` that reports nothing
/// installed, and the system folders the other copy is looked for in are
/// re-rooted under `dir`, where the test puts nothing.
fn sandbox_env(dir: &Path) -> Vec<(String, String)> {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("sandbox-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let dpkg_query = bin.join("dpkg-query");
    std::fs::write(
        &dpkg_query,
        "#!/bin/sh\necho 'dpkg-query: no packages found matching' \"$@\" >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&dpkg_query, std::fs::Permissions::from_mode(0o755)).unwrap();
    let system = dir.join("system");
    std::fs::create_dir_all(&system).unwrap();
    vec![
        ("PATH".into(), bin.to_str().expect("UTF-8 path").into()),
        (
            "DOT_AGENT_DECK_TEST_SYSTEM_ROOT".into(),
            system.to_str().expect("UTF-8 path").into(),
        ),
    ]
}

fn wait_for_file(path: &Path, want: &[u8], timeout: Duration) -> bool {
    common::wait_until(timeout, || {
        std::fs::read(path).is_ok_and(|bytes| bytes == want)
    })
}

/// The TUI as a copy at `exe`, against `server`, isolated by [`sandbox_env`]
/// under `sandbox`.
fn deck(server: &FakeReleases, exe: &Path, sandbox: &Path, extra: &[(&str, &str)]) -> TuiDeck {
    let mut builder = TuiDeck::builder().with_pty_size(200, 50);
    for (key, value) in sandbox_env(sandbox) {
        builder = builder.with_env(key, value);
    }
    for (key, value) in server.env(exe, OLD_VERSION) {
        builder = builder.with_env(key, value);
    }
    for (key, value) in extra {
        builder = builder.with_env(*key, *value);
    }
    builder.launch_with_fixture("minimal")
}

/// Run `dot-agent-deck upgrade <args>` against `server`, as a copy at `exe`,
/// with `home` as its HOME and isolated by [`sandbox_env`] under it.
fn run_upgrade(server: &FakeReleases, exe: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(bin());
    command.arg("upgrade").args(args).env_clear();
    command.envs(sandbox_env(home));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("DOT_AGENT_DECK_STATE_DIR", home.join("state"))
        .env("DOT_AGENT_DECK_SOCKET", home.join("hook.sock"))
        .env("DOT_AGENT_DECK_ATTACH_SOCKET", home.join("attach.sock"));
    for (key, value) in server.env(exe, OLD_VERSION) {
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

    let deck = deck(&server, &exe, dir.path(), &[]);
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
    let dir = common::harness_tempdir().expect("tempdir");

    let deck = deck(&server, &exe, dir.path(), &[]);
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
        dir.path(),
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
        stdout.contains(
            "Build provenance will NOT be checked: the GitHub CLI (`gh`) is not installed."
        ),
        "the sandbox PATH holds no gh, whatever the host has\n{stdout}"
    );
    assert!(
        !stdout.contains("Agent Deck (desktop app)"),
        "no desktop app is found outside the sandbox\n{stdout}"
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

/// Scenario: Run `dot-agent-deck upgrade --yes` against a fake release server whose download has the right checksum but answers `--version` with a different version than the release. The upgrade fails with the core's version-mismatch message, exits non-zero, and leaves the old file byte for byte.
#[spec("upgrade/cli-upgrade/004")]
#[test]
fn cli_upgrade_004_a_download_reporting_the_wrong_version_leaves_the_old_binary() {
    let version = RELEASE;
    let wrong = "1.2.3";
    let (asset, manifest) = release_reporting(wrong, true);
    let server = FakeReleases::start(version, asset, manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());
    let old = std::fs::read(&exe).unwrap();

    let out = run_upgrade(&server, &exe, dir.path(), &["--yes"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a wrong version must fail\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(&format!(
            "The downloaded binary reports v{wrong} instead of dot-agent-deck {version}. Nothing was changed."
        )),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        old,
        "the old binary stays byte for byte"
    );
}

/// Scenario: Run `dot-agent-deck upgrade --check` on Linux amd64 with a desktop `.deb` "installed" inside the test's own re-rooted system folder: a bundled CLI at `<root>/usr/bin/dot-agent-deck` that answers `--version`, and a sandbox `dpkg-query` that reports the package installed and owning it. The plan shows a second section for the desktop app, found there and nowhere else.
#[spec("upgrade/cli-upgrade/005")]
#[test]
fn cli_upgrade_005_the_other_copy_is_looked_for_inside_the_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    if fake_releases::platform() != Some(dot_agent_deck::self_upgrade::Platform::LinuxAmd64) {
        eprintln!("SKIP: the desktop .deb ships for Linux amd64 only");
        return;
    }
    let version = RELEASE;
    let (asset, manifest) = release(true);
    let server = FakeReleases::start(version, asset, manifest);
    let dir = common::harness_tempdir().expect("tempdir");
    let exe = writable_install(dir.path());

    // `sandbox_env` writes its `dpkg-query`; this one replaces it after.
    let env = sandbox_env(dir.path());
    let bundled = dir.path().join("system/usr/bin/dot-agent-deck");
    std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
    std::fs::write(&bundled, release_script(OLD_VERSION)).unwrap();
    std::fs::set_permissions(&bundled, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dpkg_query = dir.path().join("sandbox-bin/dpkg-query");
    std::fs::write(
        &dpkg_query,
        "#!/bin/sh\ncase \"$1\" in\n  -W) printf 'ii ' ;;\n  -S) echo \"agent-deck: $2\" ;;\n  *) exit 1 ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&dpkg_query, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut command = Command::new(bin());
    command
        .args(["upgrade", "--check"])
        .env_clear()
        .envs(env)
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("DOT_AGENT_DECK_STATE_DIR", dir.path().join("state"));
    for (key, value) in server.env(&exe, OLD_VERSION) {
        command.env(key, value);
    }
    let out = command.output().expect("run dot-agent-deck upgrade");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "Agent Deck (desktop app): update available: v{version} (current: v{OLD_VERSION})"
        )),
        "{stdout}"
    );
    assert!(
        stdout.contains("Installed from the Agent Deck `.deb`"),
        "{stdout}"
    );
}
