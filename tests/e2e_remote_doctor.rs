#![cfg(all(feature = "e2e", unix))]

//! L2 subprocess coverage for `dot-agent-deck remote doctor <name>` (PRD #345).
//!
//! These tests run the real binary with every state path redirected into a
//! test-owned directory. A synthetic `ssh` at the front of `PATH` records its
//! argv and returns deterministic observations for `ssh -G`, remote `sshd -T`,
//! the existing version/protocol probes, and any live-forward probe.
//!
//! The output contract intentionally leaves prose open: one result per line in
//! a shape such as `FAIL  AllowTcpForwarding  <headline>  <fix>`, where each
//! line contains a stable check identity and one verdict token. Tests normalize
//! whitespace/case around check identities and do not pin complete sentences.

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
use spec::spec;

const CHECKS: &[&str] = &[
    "HostReachable",
    "RemoteBinary",
    "ProtocolCompatible",
    "RemoteForward",
    "DynamicForward",
    "ExitOnForwardFailure",
    "AllowTcpForwarding",
    "ClientAliveInterval",
    "ForwardBound",
    "ForwardAgent",
];

const VERDICTS: &[&str] = &["PASS", "WARN", "FAIL", "UNKNOWN"];

/// Synthetic ssh contract:
///
/// - any argv containing `-G` returns resolved client configuration;
/// - a remote command containing `sshd -T` returns remote sshd policy;
/// - the deck's `--version` and `daemon hello` probes return parseable output;
/// - any other remote command is treated as a live-forward probe and succeeds
///   in healthy scenarios;
/// - `blocked` and `collision` deliberately give every ordinary connection the
///   SAME client-side forward error. Only `sshd -T` distinguishes them (`no`
///   versus `yes`), which pins PRD #345's headline diagnostic behavior.
const SSH_STUB_SCRIPT: &str = r#"#!/bin/sh
{
    printf 'ssh'
    for arg in "$@"; do
        printf '\t%s' "$arg"
    done
    printf '\n'
} >> "$SSH_STUB_LOG"

for arg in "$@"; do
    if [ "$arg" = "-G" ]; then
        printf '%s\n' \
            'host prod.example.test' \
            'hostname prod.example.test' \
            'user deck' \
            'port 2222' \
            'remoteforward 1080 [socks]:0' \
            'exitonforwardfailure yes' \
            'forwardagent no'
        exit 0
    fi
done

case " $* " in
    *" sshd -T "*)
        case "$SSH_STUB_SCENARIO" in
            blocked)
                printf '%s\n' 'allowtcpforwarding no' 'clientaliveinterval 30'
                exit 0
                ;;
            sshd-unavailable)
                printf '%s\n' 'sshd: Permission denied while reading host keys' >&2
                exit 1
                ;;
            *)
                printf '%s\n' 'allowtcpforwarding yes' 'clientaliveinterval 30'
                exit 0
                ;;
        esac
        ;;
esac

case "$SSH_STUB_SCENARIO" in
    blocked|collision)
        printf '%s\n' 'Error: remote port forwarding failed for listen port 1080' >&2
        exit 255
        ;;
esac

case " $* " in
    *"dot-agent-deck --version"*)
        printf 'dot-agent-deck %s\n' "$SSH_STUB_VERSION"
        exit 0
        ;;
    *"dot-agent-deck daemon hello"*)
        printf '{"ok":true,"server_version":%s}\n' "$SSH_STUB_PROTOCOL"
        exit 0
        ;;
esac

# A future implementation may use any read-only loopback query for the live
# bound check. Its exit status is the observation; no stdout shape is imposed.
exit 0
"#;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_dot-agent-deck")
}

struct DoctorFixture {
    _tempdir: tempfile::TempDir,
    home: PathBuf,
    bindir: PathBuf,
    remotes_file: PathBuf,
    session_file: PathBuf,
    ssh_config: PathBuf,
    known_hosts: PathBuf,
    argv_log: PathBuf,
}

impl DoctorFixture {
    fn new() -> Self {
        let tempdir = common::race_safe_tempdir();
        let root = tempdir.path();
        let home = root.join("home");
        let bindir = root.join("bin");
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&bindir).expect("create stub bin directory");
        std::fs::create_dir_all(&ssh_dir).expect("create staged .ssh directory");

        let ssh_stub = bindir.join("ssh");
        std::fs::write(&ssh_stub, SSH_STUB_SCRIPT).expect("write ssh stub");
        std::fs::set_permissions(&ssh_stub, std::fs::Permissions::from_mode(0o755))
            .expect("make ssh stub executable");

        let remotes_file = root.join("remotes.toml");
        std::fs::write(
            &remotes_file,
            "[[remotes]]\n\
             name = \"prod\"\n\
             type = \"ssh\"\n\
             host = \"deck@prod.example.test\"\n\
             port = 2222\n\
             version = \"0.1.0\"\n\
             added_at = \"2026-08-26T00:00:00Z\"\n",
        )
        .expect("write staged remotes.toml");

        let session_file = root.join("session.toml");
        std::fs::write(
            &session_file,
            "[[panes]]\ndir = \"/tmp/read-only-sentinel\"\nname = \"keep-me\"\ncommand = \"sleep 1\"\n",
        )
        .expect("write staged session.toml");

        let ssh_config = ssh_dir.join("config");
        std::fs::write(
            &ssh_config,
            "Host prod.example.test\n  User deck\n  RemoteForward 1080\n  ExitOnForwardFailure yes\n  ForwardAgent no\n",
        )
        .expect("write staged ssh config");

        let known_hosts = ssh_dir.join("known_hosts");
        std::fs::write(
            &known_hosts,
            "prod.example.test ssh-ed25519 READ_ONLY_TEST_KEY\n",
        )
        .expect("write staged known_hosts");

        let argv_log = root.join("ssh-argv.log");

        Self {
            _tempdir: tempdir,
            home,
            bindir,
            remotes_file,
            session_file,
            ssh_config,
            known_hosts,
            argv_log,
        }
    }

    fn run(&self, name: &str, scenario: &str) -> (Output, String) {
        let mut path_entries = vec![self.bindir.clone()];
        if let Some(path) = std::env::var_os("PATH") {
            path_entries.extend(std::env::split_paths(&path));
        }
        let path: OsString = std::env::join_paths(path_entries).expect("join isolated PATH");

        let mut cmd = Command::new(bin());
        cmd.args(["remote", "doctor", name]);
        cmd.env_clear();
        cmd.env("PATH", path);
        cmd.env("HOME", &self.home);
        cmd.env("TERM", "xterm-256color");
        cmd.env("DOT_AGENT_DECK_REMOTES", &self.remotes_file);
        cmd.env("DOT_AGENT_DECK_SESSION", &self.session_file);
        cmd.env("DOT_AGENT_DECK_SOCKET", self.home.join("hook.sock"));
        cmd.env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            self.home.join("attach.sock"),
        );
        cmd.env("DOT_AGENT_DECK_STATE_DIR", self.home.join("state"));
        cmd.env("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "1");
        cmd.env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "30");
        cmd.env("SSH_STUB_LOG", &self.argv_log);
        cmd.env("SSH_STUB_SCENARIO", scenario);
        cmd.env("SSH_STUB_VERSION", env!("DAD_VERSION"));
        cmd.env("SSH_STUB_PROTOCOL", PROTOCOL_VERSION.to_string());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let output = cmd.output().expect("spawn dot-agent-deck remote doctor");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output, text)
    }

    fn recorded_argv(&self) -> String {
        match std::fs::read_to_string(&self.argv_log) {
            Ok(log) => log,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read ssh argv log {:?}: {error}", self.argv_log),
        }
    }
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn verdict_token(line: &str) -> Option<&'static str> {
    VERDICTS.iter().copied().find(|verdict| {
        line.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|word| word.eq_ignore_ascii_case(verdict))
    })
}

fn report_line<'a>(text: &'a str, check: &str) -> &'a str {
    let check_key = normalized(check);
    let matches: Vec<_> = text
        .lines()
        .filter(|line| normalized(line).contains(&check_key) && verdict_token(line).is_some())
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one report line carrying check {check:?} and a PASS/WARN/FAIL/UNKNOWN \
         token, found {}.\noutput:\n{text}",
        matches.len()
    );
    matches[0]
}

fn assert_complete_report(text: &str) {
    for check in CHECKS {
        let _ = report_line(text, check);
    }
}

fn assert_verdict(text: &str, check: &str, expected: &str) {
    let line = report_line(text, check);
    assert_eq!(
        verdict_token(line),
        Some(expected),
        "check {check:?} should report {expected}, but its line was:\n{line}\nfull output:\n{text}"
    );
}

/// Scenario: Register `prod`, prepend a healthy synthetic `ssh`, and run the
/// real `dot-agent-deck remote doctor prod` subprocess. It exits successfully
/// and prints exactly one verdict-bearing line for every stable check identity.
#[spec("remote/doctor/001")]
#[test]
fn remote_doctor_001_healthy_remote_reports_every_check_and_exits_zero() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "healthy");

    assert!(
        output.status.success(),
        "a healthy `remote doctor prod` must exit 0, got {:?}.\noutput:\n{text}",
        output.status.code()
    );
    assert_complete_report(&text);
}

/// Scenario: Run `remote doctor missing-deck` against a registry containing
/// only `prod`. The command rejects the unknown name, includes it in the error,
/// and returns before invoking the PATH-stubbed `ssh` even once.
#[spec("remote/doctor/002")]
#[test]
fn remote_doctor_002_unknown_remote_fails_without_invoking_ssh() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("missing-deck", "healthy");

    assert!(
        !output.status.success(),
        "an unknown remote must exit non-zero.\noutput:\n{text}"
    );
    assert!(
        text.contains("missing-deck"),
        "the error must name the unknown remote.\noutput:\n{text}"
    );
    assert!(
        fixture.recorded_argv().trim().is_empty(),
        "registry resolution must happen before any ssh probe, but ssh was invoked:\n{}",
        fixture.recorded_argv()
    );
}

/// Scenario: Give two doctor runs the same client-side forward-failure bytes;
/// in one, `sshd -T` says `AllowTcpForwarding no`, while in the other it says
/// forwarding is allowed (a port collision). The reports name different causes
/// and fail the corresponding `AllowTcpForwarding` versus `ForwardBound` check.
#[spec("remote/doctor/003")]
#[test]
fn remote_doctor_003_distinguishes_sshd_block_from_port_collision() {
    let blocked_fixture = DoctorFixture::new();
    let (blocked_status, blocked) = blocked_fixture.run("prod", "blocked");
    let collision_fixture = DoctorFixture::new();
    let (collision_status, collision) = collision_fixture.run("prod", "collision");

    assert!(
        !blocked_status.status.success(),
        "AllowTcpForwarding=no is a failed diagnosis and must exit non-zero.\noutput:\n{blocked}"
    );
    assert!(
        !collision_status.status.success(),
        "an unbound/colliding remote port is a failed diagnosis and must exit non-zero.\noutput:\n{collision}"
    );
    assert_complete_report(&blocked);
    assert_complete_report(&collision);
    assert_verdict(&blocked, "AllowTcpForwarding", "FAIL");
    assert_verdict(&collision, "ForwardBound", "FAIL");

    assert_ne!(
        blocked, collision,
        "the two scenarios intentionally return byte-identical client-side ssh errors; the \
         doctor's remote-side evidence must make their reports different"
    );
    assert!(
        blocked.to_ascii_lowercase().contains("allowtcpforwarding"),
        "the sshd-policy report must name AllowTcpForwarding.\noutput:\n{blocked}"
    );
    let collision_lower = collision.to_ascii_lowercase();
    assert!(
        collision_lower.contains("port")
            && (collision_lower.contains("bound") || collision_lower.contains("collision")),
        "the collision report must identify the unbound/colliding port without pinning prose.\n\
         output:\n{collision}"
    );
}

/// Scenario: Let every client and deck probe succeed but make remote `sshd -T`
/// exit non-zero with a permission error. Both sshd-derived checks report
/// UNKNOWN with a useful hint, never PASS, while all other checks still render.
#[spec("remote/doctor/004")]
#[test]
fn remote_doctor_004_unavailable_sshd_is_unknown_and_does_not_stop_report() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "sshd-unavailable");

    assert!(
        !output.status.success(),
        "a diagnosis containing UNKNOWN must not exit as all-clear success.\noutput:\n{text}"
    );
    assert_complete_report(&text);
    assert_verdict(&text, "AllowTcpForwarding", "UNKNOWN");
    assert_verdict(&text, "ClientAliveInterval", "UNKNOWN");

    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("sshd")
            && (lower.contains("permission")
                || lower.contains("root")
                || lower.contains("unavailable")),
        "UNKNOWN must include a useful sshd availability/permission hint.\noutput:\n{text}"
    );
}

/// Scenario: Stage registry, session, and OpenSSH client files, run a complete
/// healthy diagnosis, then compare every file byte-for-byte and inspect every
/// recorded ssh argv. The doctor probes only: it edits neither local state nor
/// remote configuration.
#[spec("remote/doctor/005")]
#[test]
fn remote_doctor_005_full_run_is_read_only() {
    let fixture = DoctorFixture::new();
    let guarded_files = [
        fixture.remotes_file.as_path(),
        fixture.session_file.as_path(),
        fixture.ssh_config.as_path(),
        fixture.known_hosts.as_path(),
    ];
    let before: Vec<_> = guarded_files
        .iter()
        .map(|path| std::fs::read(path).unwrap_or_else(|error| panic!("read {path:?}: {error}")))
        .collect();

    let (output, text) = fixture.run("prod", "healthy");
    assert!(
        output.status.success(),
        "the healthy read-only run must complete successfully.\noutput:\n{text}"
    );

    for (path, expected) in guarded_files.iter().zip(before) {
        let actual = std::fs::read(path)
            .unwrap_or_else(|error| panic!("read {path:?} after doctor: {error}"));
        assert_eq!(
            actual, expected,
            "`remote doctor` modified staged read-only input {path:?}"
        );
    }

    let argv = fixture.recorded_argv();
    assert!(
        !argv.trim().is_empty(),
        "a full diagnosis should invoke the synthetic ssh probes"
    );
    let argv_lower = argv.to_ascii_lowercase();
    for mutating_form in [
        " rm ",
        " mv ",
        " cp ",
        " touch ",
        " mkdir ",
        " chmod ",
        " chown ",
        " sed -i",
        " tee ",
        "systemctl ",
        "service ",
        "sshd_config",
        ">>",
    ] {
        assert!(
            !argv_lower.contains(mutating_form),
            "read-only doctor invoked ssh with mutating form {mutating_form:?}:\n{argv}"
        );
    }
}
