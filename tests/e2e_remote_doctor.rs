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
/// - a command mentioning `/dev/tcp/127.0.0.1/1080` is the live-forward probe.
///   Its stdout is the hex rendering of whatever the listener replied, and the
///   probe must normalise whitespace before reading it so `05 00`, ` 05 00`
///   and `0500` are the same observation:
///   `healthy` writes `05 00` and exits 0 (a SOCKS5 no-auth acceptance),
///   `collision`/`squatter` write `48 54` and exit 0 (a foreign service that
///   answered with something else), `squatter-silent` writes NOTHING and exits
///   0 (a foreign service that connected and never replied),
///   `nothing-listening`/`blocked` exit 1 with a refusal, and
///   `probe-unavailable` exits 127. An unlisted scenario exits 3 rather than
///   defaulting, so a typo cannot masquerade as a silent listener;
/// - `non-dynamic` resolves a concrete reverse forward whose listening port
///   accepts a connection but cannot be attributed to this user's tunnel;
/// - `blocked` differs from `collision` through both independent observations:
///   `sshd -T` says forwarding is disabled and no listener answers for the
///   former, while policy permits forwarding but a non-SOCKS squatter answers
///   for the latter. Neither relies on an observation ssh session attempting
///   the configured forward itself.
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
        case "$SSH_STUB_SCENARIO" in
            non-dynamic)
                forward='remoteforward 1080 db.internal.test:5432'
                ;;
            *)
                forward='remoteforward 1080 [socks]:0'
                ;;
        esac
        case "$SSH_STUB_SCENARIO" in
            warn-forward-agent)
                forward_agent=yes
                ;;
            *)
                forward_agent=no
                ;;
        esac
        printf '%s\n' \
            'host prod.example.test' \
            'hostname prod.example.test' \
            'user deck' \
            'port 2222' \
            "$forward" \
            'exitonforwardfailure yes' \
            "forwardagent $forward_agent"
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

# Match the endpoint, not the shell byte-plumbing. The stdout is deliberately
# the semantic observation a future probe must parse: the hex bytes the
# listener replied with, or nothing at all when it replied with nothing.
case " $* " in
    *"/dev/tcp/127.0.0.1/1080"*)
        case "$SSH_STUB_SCENARIO" in
            healthy|sshd-unavailable|warn-forward-agent)
                printf '%s\n' '05 00'
                exit 0
                ;;
            collision|squatter|non-dynamic)
                printf '%s\n' '48 54'
                exit 0
                ;;
            squatter-silent)
                exit 0
                ;;
            nothing-listening|blocked)
                printf '%s\n' 'bash: connect: Connection refused' >&2
                exit 1
                ;;
            probe-unavailable)
                printf '%s\n' 'bash: command not found' >&2
                exit 127
                ;;
            *)
                printf 'ssh stub: no probe behaviour for scenario %s\n' \
                    "$SSH_STUB_SCENARIO" >&2
                exit 3
                ;;
        esac
        ;;
esac

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
        self.run_with_protocol(name, scenario, PROTOCOL_VERSION)
    }

    /// Same run, with the protocol version the stub remote reports made
    /// explicit. Issue #491: the doctor must no longer key any verdict on how
    /// that number compares to this binary's own.
    fn run_with_protocol(
        &self,
        name: &str,
        scenario: &str,
        remote_protocol: u32,
    ) -> (Output, String) {
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
        cmd.env("SSH_STUB_PROTOCOL", remote_protocol.to_string());
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

fn assert_exit_code(output: &Output, expected: i32, text: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "expected exit code {expected}, got {:?}.\noutput:\n{text}",
        output.status.code()
    );
}

fn assert_overall(text: &str, expected: &str) {
    assert!(
        text.lines().any(|line| line
            .trim()
            .eq_ignore_ascii_case(&format!("Overall: {expected}"))),
        "expected Overall: {expected}.\noutput:\n{text}"
    );
}

fn assert_live_probe_was_run(fixture: &DoctorFixture) {
    let argv = fixture.recorded_argv();
    assert!(
        argv.contains("/dev/tcp/127.0.0.1/1080"),
        "the live-forward observation must target the resolved endpoint.\nssh argv:\n{argv}"
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

    assert_exit_code(&output, 0, &text);
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

/// Scenario: Run one diagnosis where `sshd -T` forbids forwarding and no
/// listener answers, then another where policy permits forwarding but a
/// non-SOCKS service occupies the port. The reports name their distinct causes
/// and fail the corresponding `AllowTcpForwarding` versus `ForwardBound` check.
#[spec("remote/doctor/003")]
#[test]
fn remote_doctor_003_distinguishes_sshd_block_from_port_collision() {
    let blocked_fixture = DoctorFixture::new();
    let (blocked_status, blocked) = blocked_fixture.run("prod", "blocked");
    let collision_fixture = DoctorFixture::new();
    let (collision_status, collision) = collision_fixture.run("prod", "collision");

    assert_exit_code(&blocked_status, 1, &blocked);
    assert_exit_code(&collision_status, 1, &collision);
    assert_complete_report(&blocked);
    assert_complete_report(&collision);
    assert_verdict(&blocked, "AllowTcpForwarding", "FAIL");
    assert_verdict(&collision, "ForwardBound", "FAIL");
    assert_live_probe_was_run(&blocked_fixture);
    assert_live_probe_was_run(&collision_fixture);

    // The discriminator, stated as a verdict rather than as prose: policy is
    // the cause in one run and is explicitly fine in the other.
    assert_verdict(&collision, "AllowTcpForwarding", "PASS");

    assert_ne!(
        blocked, collision,
        "independent sshd policy and listener-identity observations must make the reports \
         different"
    );
    // Scoped to the ForwardBound lines on purpose: the whole-report comparison
    // above is satisfied by the AllowTcpForwarding line alone, so without this
    // the two runs could hand the user the SAME liveness sentence.
    assert_ne!(
        report_line(&blocked, "ForwardBound"),
        report_line(&collision, "ForwardBound"),
        "each run must explain the liveness result by its own cause.\nblocked:\n{blocked}\n\
         collision:\n{collision}"
    );

    let collision_line = report_line(&collision, "ForwardBound").to_ascii_lowercase();
    assert!(
        collision_line.contains("1080"),
        "the collision line must name the port it is talking about.\nline:\n{}\n\
         output:\n{collision}",
        report_line(&collision, "ForwardBound")
    );
    assert!(
        [
            "collision",
            "foreign",
            "something else",
            "not this",
            "taken",
            "another"
        ]
        .iter()
        .any(|marker| collision_line.contains(marker)),
        "the collision line must say the listener is not this tunnel's, without pinning prose.\n\
         line:\n{}\nfull output:\n{collision}",
        report_line(&collision, "ForwardBound")
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

    assert_exit_code(&output, 2, &text);
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
/// healthy diagnosis through the recording ssh script, then compare every file
/// byte-for-byte and inspect every recorded argv for deck-issued mutations.
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

/// Scenario: Keep every remote observation healthy except for a foreign
/// service occupying the configured reverse-dynamic port. The talkative
/// squatter, which answers with non-SOCKS bytes, fails `ForwardBound`, makes
/// the overall result FAIL, and exits 1; a second run with a silent squatter
/// that accepts the connection and never replies is likewise never PASS and
/// never exits 0.
#[spec("remote/doctor/006")]
#[test]
fn remote_doctor_006_squatter_collision_never_reports_pass_or_exit_zero() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "squatter");

    assert_live_probe_was_run(&fixture);
    assert_verdict(&text, "ForwardBound", "FAIL");
    assert_overall(&text, "FAIL");
    assert_exit_code(&output, 1, &text);

    // A squatter that accepts the connection and then says nothing is the same
    // collision seen through a quieter service, and it is the shape most likely
    // to be read as success by a probe that only checks the connect. Pinned
    // loosely — whether "connected but silent" is FAIL or UNKNOWN is an honest
    // judgement call; a confident PASS is not.
    let silent_fixture = DoctorFixture::new();
    let (silent_output, silent) = silent_fixture.run("prod", "squatter-silent");

    assert_live_probe_was_run(&silent_fixture);
    assert_ne!(
        verdict_token(report_line(&silent, "ForwardBound")),
        Some("PASS"),
        "a listener that never answered the handshake is not a verified tunnel.\noutput:\n{silent}"
    );
    assert_ne!(
        silent_output.status.code(),
        Some(0),
        "an unverified listener must not exit as clear.\noutput:\n{silent}"
    );
}

/// Scenario: A healthy reverse-dynamic listener accepts the remote probe and
/// answers a SOCKS5 no-auth handshake with `05 00`. `ForwardBound` identifies
/// that verified SOCKS listener as PASS, the overall result is PASS, and exit is 0.
#[spec("remote/doctor/007")]
#[test]
fn remote_doctor_007_verified_socks_tunnel_reports_pass_and_exits_zero() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "healthy");

    assert_live_probe_was_run(&fixture);
    assert_verdict(&text, "ForwardBound", "PASS");
    let line = report_line(&text, "ForwardBound").to_ascii_lowercase();
    assert!(
        line.contains("socks")
            && (line.contains("verified")
                || line.contains("handshake")
                || line.contains("confirmed")),
        "a confident PASS must say the SOCKS listener was verified, not merely reachable.\n\
         line:\n{}\nfull output:\n{text}",
        report_line(&text, "ForwardBound")
    );
    assert_overall(&text, "PASS");
    assert_exit_code(&output, 0, &text);
}

/// Scenario: The configured reverse-dynamic port refuses the live probe while
/// every static configuration check is healthy. `ForwardBound` and the overall
/// result are not PASS, because no live tunnel was observed.
#[spec("remote/doctor/008")]
#[test]
fn remote_doctor_008_nothing_listening_never_reports_pass() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "nothing-listening");

    assert_live_probe_was_run(&fixture);
    assert_ne!(
        verdict_token(report_line(&text, "ForwardBound")),
        Some("PASS"),
        "a refused connection cannot verify the user's tunnel.\noutput:\n{text}"
    );
    assert!(
        !output.status.success(),
        "an unobserved live tunnel must not exit as clear.\noutput:\n{text}"
    );
}

/// Scenario: The remote lacks usable tooling for the live probe while every
/// other observation is healthy. `ForwardBound` and the overall result are
/// UNKNOWN rather than PASS, and the incomplete diagnosis exits 2.
#[spec("remote/doctor/009")]
#[test]
fn remote_doctor_009_unavailable_probe_tooling_is_unknown_and_exits_two() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "probe-unavailable");

    assert_live_probe_was_run(&fixture);
    assert_verdict(&text, "ForwardBound", "UNKNOWN");
    assert_overall(&text, "UNKNOWN");
    assert_exit_code(&output, 2, &text);
}

/// Scenario: `ssh -G` resolves a concrete reverse forward and something accepts
/// the live TCP connection. Because that observation cannot attribute the
/// listener to this user's tunnel, `ForwardBound` is UNKNOWN and exit is 2.
#[spec("remote/doctor/010")]
#[test]
fn remote_doctor_010_non_dynamic_listener_is_unattributable_and_unknown() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "non-dynamic");

    assert_live_probe_was_run(&fixture);
    assert_verdict(&text, "ForwardBound", "UNKNOWN");
    let line = report_line(&text, "ForwardBound").to_ascii_lowercase();
    assert!(
        line.contains("attribute") || line.contains("verify") || line.contains("ownership"),
        "the caveat must say why an accepting listener is not a confident PASS.\n\
         line:\n{}\nfull output:\n{text}",
        report_line(&text, "ForwardBound")
    );
    assert_overall(&text, "UNKNOWN");
    assert_exit_code(&output, 2, &text);
}

/// Scenario: Every diagnostic observation is healthy except that `ssh -G`
/// resolves `ForwardAgent yes`. The advisory is the worst verdict, so the
/// overall report is WARN and still exits 0.
#[spec("remote/doctor/011")]
#[test]
fn remote_doctor_011_warn_only_report_exits_zero() {
    let fixture = DoctorFixture::new();
    let (output, text) = fixture.run("prod", "warn-forward-agent");

    assert_verdict(&text, "ForwardAgent", "WARN");
    assert_overall(&text, "WARN");
    assert_exit_code(&output, 0, &text);
}

/// Scenario: An otherwise healthy remote whose `daemon hello` reports an attach
/// protocol version far from this binary's own. `ProtocolCompatible` passes and
/// the run exits 0, because the remote's TUI and daemon are one install and the
/// laptop is only ssh plus a terminal — the two constants never share a wire.
#[spec("remote/doctor/012")]
#[test]
fn remote_doctor_012_differing_remote_protocol_is_not_a_fault() {
    let fixture = DoctorFixture::new();

    // Both directions, since the removed comparison had a distinct arm and a
    // distinct (equally spurious) remedy for each: "upgrade the remote" below,
    // "upgrade your laptop binary" above.
    for remote_protocol in [PROTOCOL_VERSION.saturating_sub(1), PROTOCOL_VERSION + 1] {
        let (output, text) = fixture.run_with_protocol("prod", "healthy", remote_protocol);

        assert_verdict(&text, "ProtocolCompatible", "PASS");
        assert_exit_code(&output, 0, &text);
        assert_complete_report(&text);
        assert!(
            !normalized(&text).contains(&normalized("laptop speaks")),
            "no laptop-side protocol version may be quoted at the user \
             (issue #491), but the report for remote protocol \
             {remote_protocol} says:\n{text}"
        );
    }
}
