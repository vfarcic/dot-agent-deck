#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for the **remote transport**: a deck reached through a
//! real `ssh -N -L` child and a real forwarded Unix socket, observed beside a
//! local deck.
//!
//! # What this proves that nothing else does
//!
//! Every other fleet test — `tests/e2e_fleet_observation.rs` here, the desktop
//! crate's `RealDeck` tests in the fast tier — uses [`Endpoint::Local`], for
//! which `EndpointConnection::open` returns `Ok(Self::Local(..))` with no
//! socket, no I/O and no peer. So nothing anywhere exercised
//! [`Endpoint::Remote`] end to end: not the `ssh` child, not the forwarded
//! socket, not the tunnel's lifetime or its teardown, not
//! `EndpointConnection::open`'s real arm, and not
//! `DaemonClient::for_connection`. `docs/develop/desktop-gui.md` records the
//! remote leg as verified by a single manual run whose *display* half was
//! never checked.
//!
//! A loopback tunnel tests the **transport** completely. The daemon genuinely
//! is reached through a forwarded Unix socket that a real `ssh` client bound,
//! carrying bytes over a real SSH connection to a real `sshd`.
//!
//! # What it deliberately does NOT prove
//!
//! **That a client which resolves paths locally would be caught.** Loopback
//! shares one filesystem, so the daemon's `/var/tmp/…/attach.sock` and the
//! client's name for it are the same bytes and every path assertion passes
//! whichever side resolved it. Making the two sides *distinguishable* needs
//! divergent resolution inputs rather than a second machine; that is a
//! different test and it is called out here so nobody reads this file as
//! having covered it.
//!
//! It also does not prove anything about a **hostile** far end: the host key
//! here is one the test generated a second earlier, so the host-key check runs
//! and passes, and nothing about a check that passes tests what it refuses.
//!
//! And it does not reach **`EndpointTunnels::acquire`**, which is the one place
//! a real remote endpoint would make a deterministic stall possible. Written
//! down so the next person does not re-derive it: that type is `pub(crate)` to
//! `dot-agent-deck-desktop`, so no `tests/e2e_*.rs` file can name it — and a
//! dev-dependency the other way is ruled out for the reason
//! `tests/e2e_fleet_observation.rs`'s header gives (it would make Tauri and its
//! GTK/WebKit closure a build input of this tier on three platforms), which
//! would not reach a `pub(crate)` item anyway. The only place it can be written
//! is that crate's own `#[cfg(test)]` module, i.e. **the fast tier**, where M9's
//! four objections apply — and one of them is sharper than when M9 wrote it:
//! `acquire` calls `SshProgram::resolve()` itself, so such a test could not
//! point the stalled deck at the wrapper described below, and on a host with no
//! `ssh` at a candidate path `acquire` returns `AcquireError::Local`
//! *immediately*. The stalled deck would not stall and the test would pass
//! having asserted nothing, which is the #407 failure mode exactly. Injecting an
//! `SshProgram` is the production seam that would fix it, and it is a decision
//! rather than a detail. See `EndpointTunnels::acquire`'s own *One gate per
//! deck, and why this one has no test* section, which already states the case —
//! and note what it says about the shape: the map lock is **not** held across
//! the spawn (M3 scoped it to the lookup), so what is held there is the
//! per-deck gate.
//!
//! # The sandboxed `sshd`, and what the test is allowed to touch
//!
//! `ssh` to this host normally fails with `Permission denied (publickey)`, and
//! putting a key in the operator's `~/.ssh/authorized_keys` to fix that is not
//! a trade worth making — a test must not leave a login behind. So the test
//! runs an `sshd` **of its own**: its own host key, its own `authorized_keys`
//! naming a keypair generated in its own temp dir, its own config, on an
//! ephemeral port, as the current user. A non-root `sshd` serves same-user
//! logins, which is the only case here.
//!
//! If any of that cannot be established the test prints `SKIP: [e2e] …` and
//! returns, the way every other preflight in this harness does — and
//! `DOT_AGENT_DECK_REQUIRE_REAL_E2E=1` turns those skips into failures
//! (CLAUDE.md rule 5). A vacuous pass would be worse than no test.
//!
//! ### So this test's CI coverage is CONDITIONAL, and nothing else says so
//!
//! `e2e-deterministic` runs `cargo test-e2e` on `ubuntu-latest` and does **not**
//! set `DOT_AGENT_DECK_REQUIRE_REAL_E2E`. If a runner image ever ships without
//! an `sshd` at one of [`SSHD_CANDIDATES`], this file skips honestly: the job
//! stays green, this transport is covered by nothing, and no line of output
//! distinguishes that from a run that passed. GitHub's ubuntu images ship
//! `openssh-server`, so it almost certainly runs today — but that is an
//! inherited property of someone else's image, not something this repository
//! asserts, and it can change without anything here noticing.
//!
//! Setting the flag on that job is **not** the fix. It is tier-wide, so it
//! would promote every legitimate skip in every other e2e file to a failure,
//! which is a far larger change than making one test's coverage load-bearing.
//! The conditionality is written down here and in this test's `tests/CATALOG.md`
//! entry instead, so it is found by anyone looking for what covers the remote
//! transport rather than discovered when it is already gone.
//!
//! ## Why the `ssh` program is a wrapper, and exactly what it changes
//!
//! `forced_options()` forces `StrictHostKeyChecking=yes` — deliberately, and
//! this test would be worth much less without it. What it does **not** force,
//! also deliberately, is where the check is anchored: `UserKnownHostsFile`,
//! `GlobalKnownHostsFile` and `KnownHostsCommand` are inherited from the user's
//! own ssh config, because that is how CA- and inventory-driven fleets
//! distribute host keys.
//!
//! Inherited from *the user's*, and there is no way around that from a test:
//! measured on OpenSSH 10.2p1, `$HOME` does not move it. OpenSSH tilde-expands
//! both `Include ~/.ssh/config` and the default `~/.ssh/known_hosts` against
//! `getpwuid()->pw_dir`, so a sandbox `HOME` changes neither — `ssh -G` under
//! `env -i HOME=<sandbox>` still reported `userknownhostsfile
//! /home/<user>/.ssh/known_hosts`, and the connection failed with *"No ED25519
//! host key is known … and you have requested strict checking"*.
//!
//! That leaves exactly two ways to satisfy a forced `StrictHostKeyChecking=yes`
//! against a host key generated one second ago: write into the operator's real
//! `~/.ssh`, or hand the tunnel a different `ssh`. `SshProgram::at` exists for
//! the second — it is production API, used for a stored settings value, and its
//! own doc names it the escape hatch — so the wrapper is what this test uses.
//!
//! **The wrapper `exec`s the very binary `SshProgram::resolve()` chose**, with
//! four `-o` flags prepended and nothing else altered. Because it `exec`s, the
//! pid is preserved, so the `setsid` done in `pre_exec` and the `killpg` in
//! `RemoteTunnel::close` both still reach it. Every flag is one production
//! deliberately leaves to the user's config, and none of them weakens a check:
//!
//! * `UserKnownHostsFile` / `GlobalKnownHostsFile` — anchor the host-key check
//!   at the one file this test wrote. Strict checking stays **on** and really
//!   runs; the tunnel would not come up if the key did not match.
//! * `IdentitiesOnly` / `IdentityAgent=none` — offer the generated key and
//!   nothing else, so the run cannot depend on (or exhaust `MaxAuthTries`
//!   against) whatever the operator's agent happens to be holding.
//!
//! In other words the wrapper stands in for the `~/.ssh/config` a user of this
//! feature would already have, which the test may not write.
//!
//! ## Isolation
//!
//! `RemoteTunnel::open` sweeps `reap_orphaned_tunnels` over
//! `$XDG_RUNTIME_DIR/dot-agent-deck/tunnels` before it spawns, and that sweep
//! **deletes**. So the test points `XDG_RUNTIME_DIR` at its own directory
//! inside the harness root before anything reads it, and asserts below that the
//! forwarded socket really landed there. That is the one thing in this file
//! that could otherwise reach a developer's machine.
//!
//! Everything else follows `fleet/observe/001`: own attach socket, hook socket,
//! state dir, `HOME`, schedules path and log path per daemon, and
//! `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS=0` so no daemon exits under the client
//! mid-test.
//!
//! Lane 1: no credential and no real agent — the agents are `sh -c 'sleep 600'`
//! stand-ins, exactly as `tests/e2e_handshake.rs` and `fleet/observe/001` use.

mod common;

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{DaemonProc, spawn_daemon_serve_with_env};
use dot_agent_deck::daemon_client::{DaemonClient, Endpoint, LocalEndpoint, RemoteEndpoint};
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::platform::ipc::EndpointPresence;
use dot_agent_deck::remote_tunnel::{
    EndpointConnection, Hostname, KeyPath, RemoteSocketPath, SshProgram, check_socket_path,
    socket_file_name, tunnel_socket_dir_in,
};
use spec::spec;

/// Display names distinctive enough that finding one in the other deck's
/// listing is unambiguous, on the same rule `fleet/observe/001` uses.
const REMOTE_AGENT: &str = "sierra-remote-73";
const LOCAL_AGENT: &str = "tango-local-19";

/// Absolute paths a system `sshd` is looked for at, in the order a test should
/// prefer them. Deliberately **not** a `PATH` search: `sshd` is normally in a
/// directory that is not on an ordinary user's `PATH`, and an absolute list is
/// the same discipline `SSH_PROGRAM_CANDIDATES` applies to the client.
const SSHD_CANDIDATES: &[&str] = &[
    "/usr/sbin/sshd",
    "/usr/bin/sshd",
    "/usr/libexec/sshd",
    "/usr/local/sbin/sshd",
    "/opt/homebrew/sbin/sshd",
    "/run/current-system/sw/bin/sshd",
];

/// How long the sandboxed `sshd` is given to bind its port.
const SSHD_LISTEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How many ports are tried before giving up on starting `sshd`.
const SSHD_START_ATTEMPTS: u32 = 3;

/// How long the `ssh` child is given to disappear after the last lease drops.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// The sandboxed sshd
// ---------------------------------------------------------------------------

/// A single-purpose `sshd` owned by this test: its own host key, its own
/// `authorized_keys`, its own config, on an ephemeral loopback port.
struct SandboxSshd {
    child: std::process::Child,
    port: u16,
    /// The private key `authorized_keys` names, for `ssh -i`.
    client_key: PathBuf,
    /// The `known_hosts` file the wrapper anchors the host-key check at.
    known_hosts: PathBuf,
    log: PathBuf,
}

impl Drop for SandboxSshd {
    fn drop(&mut self) {
        // `sshd` forks a child per connection, and every one of them is in the
        // process group we asked for at spawn, so one negative-pid signal reaps
        // the lot. Best-effort: ESRCH here means it is already gone.
        let pid = self.child.id();
        // SAFETY: `kill(2)` with a negative pid signals the process group,
        // which is this `sshd`'s own — `process_group(0)` made it a leader.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl SandboxSshd {
    /// Generate the keys and config, start `sshd`, and block until it accepts a
    /// TCP connection.
    ///
    /// `Err` is a *reason*, shaped for [`common::skip_unless`] — every failure
    /// here is "this host cannot run a private sshd", never a defect in the
    /// code under test.
    fn start(dir: &Path, sshd: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create the sshd sandbox: {e}"))?;
        let host_key = dir.join("hostkey");
        let client_key = dir.join("clientkey");
        keygen(&host_key)?;
        keygen(&client_key)?;

        let client_pub = std::fs::read_to_string(client_key.with_extension("pub"))
            .map_err(|e| format!("read the generated client public key: {e}"))?;
        let authorized_keys = dir.join("authorized_keys");
        write_private(&authorized_keys, client_pub.as_bytes())?;

        // One attempt per port: the window between choosing a free port and
        // `sshd` binding it is real, and a lost race must not read as "this
        // host cannot run sshd". A retry over *ports*, not a poll over time —
        // the wait itself is `common::wait_until` inside `await_listening`.
        let mut last = String::new();
        for attempt in 1..=SSHD_START_ATTEMPTS {
            let port = free_loopback_port()?;
            let config = dir.join("sshd_config");
            std::fs::write(&config, sshd_config(&host_key, &authorized_keys, port))
                .map_err(|e| format!("write the sshd config: {e}"))?;
            let log = dir.join("sshd.log");

            let mut command = Command::new(sshd);
            command
                .arg("-D") // foreground, so the pid we hold is the daemon
                .arg("-f")
                .arg(&config)
                .arg("-E")
                .arg(&log)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            let mut child = command
                .spawn()
                .map_err(|e| format!("spawn {}: {e}", sshd.display()))?;

            match await_listening(&mut child, port) {
                Ok(()) => {
                    let known_hosts = dir.join("known_hosts");
                    write_known_hosts(&known_hosts, &host_key, port)?;
                    return Ok(Self {
                        child,
                        port,
                        client_key,
                        known_hosts,
                        log,
                    });
                }
                Err(reason) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let tail = std::fs::read_to_string(&log).unwrap_or_default();
                    last = format!("attempt {attempt}: {reason}; sshd said: {}", tail.trim());
                }
            }
        }
        Err(format!("a sandboxed sshd would not come up: {last}"))
    }

    /// Prove key auth and the anchored host-key check both work **before** the
    /// tunnel depends on them, so an environment problem is a named skip rather
    /// than an opaque `TunnelError` thirty seconds later.
    fn smoke(&self, wrapper: &Path) -> Result<(), String> {
        let output = Command::new(wrapper)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-i")
            .arg(&self.client_key)
            .arg("--")
            .arg("127.0.0.1")
            .arg("true")
            .output()
            .map_err(|e| format!("run the ssh wrapper: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "a same-user pubkey login to the sandboxed sshd failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The `sshd_config` this test runs under, as text.
///
/// Everything here is about being a *private* sshd: its own key material, its
/// own port, no PAM, no password auth, and no session features the tunnel does
/// not need. `StrictModes no` because the key files live in a temp dir rather
/// than under `~/.ssh`, which is the whole point. `AllowStreamLocalForwarding`
/// is the one that matters for the forward itself — `-L <sock>:<sock>` is a
/// Unix-domain forward, not a TCP one.
fn sshd_config(host_key: &Path, authorized_keys: &Path, port: u16) -> String {
    format!(
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {}\n\
         AuthorizedKeysFile {}\n\
         StrictModes no\n\
         UsePAM no\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         PermitUserEnvironment no\n\
         AllowTcpForwarding yes\n\
         AllowStreamLocalForwarding yes\n\
         X11Forwarding no\n\
         PrintMotd no\n\
         LogLevel VERBOSE\n",
        host_key.display(),
        authorized_keys.display(),
    )
}

/// `ssh-keygen` an unencrypted ed25519 keypair at `path`.
fn keygen(path: &Path) -> Result<(), String> {
    let output = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-C", "dot-agent-deck-e2e"])
        .arg("-f")
        .arg(path)
        .output()
        .map_err(|e| format!("run ssh-keygen: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ssh-keygen failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The one `known_hosts` line the wrapper anchors the host-key check at.
///
/// Built from the generated host key rather than scraped with `ssh-keyscan`,
/// which would ask the server what its key is and then believe the answer —
/// the shape of trust this file must not demonstrate. The `[host]:port` form is
/// how OpenSSH keys a non-default port.
fn write_known_hosts(path: &Path, host_key: &Path, port: u16) -> Result<(), String> {
    let public = std::fs::read_to_string(host_key.with_extension("pub"))
        .map_err(|e| format!("read the generated host public key: {e}"))?;
    let mut fields = public.split_whitespace();
    let algorithm = fields
        .next()
        .ok_or("the generated host public key has no algorithm field")?;
    let material = fields
        .next()
        .ok_or("the generated host public key has no key field")?;
    std::fs::write(path, format!("[127.0.0.1]:{port} {algorithm} {material}\n"))
        .map_err(|e| format!("write the test known_hosts: {e}"))
}

/// Write `bytes` to `path` at 0600, which is what `ssh` and `sshd` require of
/// key material they are pointed at.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// A loopback port nothing is listening on right now.
fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("bind a loopback port to choose a free one: {e}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("read the chosen port: {e}"))
}

/// Block until `port` accepts, failing fast if `child` dies first.
///
/// The wait is [`common::wait_until`] rather than a loop of its own (Decision
/// 21). The child is behind a `RefCell` because that helper takes an `Fn`, and
/// losing the fail-fast would cost the whole timeout on every attempt against a
/// host where `sshd` cannot start at all — which is exactly the host this
/// function exists to diagnose quickly.
fn await_listening(child: &mut std::process::Child, port: u16) -> Result<(), String> {
    let exited: std::cell::Cell<Option<String>> = std::cell::Cell::new(None);
    let probe = std::cell::RefCell::new(child);
    let settled = common::wait_until(SSHD_LISTEN_TIMEOUT, || {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        match probe.borrow_mut().try_wait() {
            Ok(Some(status)) => {
                exited.set(Some(format!("sshd exited before listening ({status})")));
                true
            }
            Ok(None) => false,
            Err(e) => {
                exited.set(Some(format!("could not query the sshd child: {e}")));
                true
            }
        }
    });
    if let Some(reason) = exited.take() {
        return Err(reason);
    }
    if settled {
        Ok(())
    } else {
        Err(format!(
            "sshd did not accept on 127.0.0.1:{port} within {}s",
            SSHD_LISTEN_TIMEOUT.as_secs()
        ))
    }
}

/// `path` as one `/bin/sh` word: wrapped in single quotes, with any embedded
/// single quote closed, escaped and reopened.
///
/// Single quoting rather than double, because inside single quotes `sh` gives
/// no character any meaning at all — there is nothing left to think about for
/// `$`, a backtick or a backslash. The `'\''` dance is the only case single
/// quotes cannot express directly.
fn sh_word(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// Write the `ssh` wrapper described in this module's header and return its
/// path. `real` is whatever `SshProgram::resolve()` chose.
///
/// Both interpolated paths go through [`sh_word`]. Neither can hold anything
/// interesting today — the `ssh` binary comes from `SSH_PROGRAM_CANDIDATES`,
/// which is a list of absolute literals, and `known_hosts` is a file this test
/// made under the harness temp root — but "the temp root has no spaces in it"
/// is an assumption that is invisible from the call site, and quoting costs a
/// pair of characters to stop making it.
fn write_ssh_wrapper(path: &Path, real: &Path, known_hosts: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let script = format!(
        "#!/bin/sh\n\
         # Generated by tests/e2e_remote_deck_transport.rs. Stands in for the\n\
         # ~/.ssh/config a user of this feature would have, which a test may\n\
         # not write. `exec` so the pid survives for killpg; nothing else is\n\
         # altered.\n\
         exec {} \\\n\
         \x20 -o UserKnownHostsFile={} \\\n\
         \x20 -o GlobalKnownHostsFile=/dev/null \\\n\
         \x20 -o IdentitiesOnly=yes \\\n\
         \x20 -o IdentityAgent=none \\\n\
         \x20 \"$@\"\n",
        sh_word(real),
        sh_word(known_hosts),
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(path)
        .map_err(|e| format!("create the ssh wrapper: {e}"))?;
    file.write_all(script.as_bytes())
        .map_err(|e| format!("write the ssh wrapper: {e}"))
}

/// Start one long-lived stand-in agent on `daemon` and block until its registry
/// reports it. Same helper, same stand-in, as `fleet/observe/001`.
fn start_stand_in(daemon: &DaemonProc, display_name: &str, pane_id: &str) {
    let response = daemon
        .send_attach_request(&AttachRequest::StartAgent {
            command: Some("sh -c 'sleep 600'".into()),
            cwd: None,
            rows: 24,
            cols: 80,
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), pane_id.into())],
            display_name: Some(display_name.into()),
            tab_membership: None,
            agent_type: None,
            seed: None,
        })
        .expect("StartAgent over the attach socket");
    assert!(
        response.error.is_none(),
        "StartAgent should succeed, got error: {:?}",
        response.error
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(
        records.len(),
        1,
        "the stand-in agent must be registered before the deck is observed"
    );
}

/// Everything that has to be true of the host before the tunnel is attempted,
/// as one reason string.
fn preflight(sandbox: &Path) -> Result<(SshProgram, PathBuf), String> {
    // The production resolver, so "no ssh here" is decided by exactly the list
    // the app ships rather than by a second opinion.
    let real = SshProgram::resolve().map_err(|e| e.to_string())?;
    let sshd = SSHD_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "no sshd at any of {} — a private sshd is what makes a loopback \
                 tunnel possible without touching the operator's account",
                SSHD_CANDIDATES.join(", ")
            )
        })?
        .to_path_buf();
    // `sun_path` is 104 bytes and the forwarded socket is the longest path in
    // the run. Checked with the production predicate, against a name built the
    // production way, so a deep temp root reads as "cannot run here" rather
    // than as a tunnel that mysteriously refuses to open.
    let dir = tunnel_socket_dir_in(Some(sandbox)).map_err(|e| e.to_string())?;
    check_socket_path(&dir.join(socket_file_name())).map_err(|e| e.to_string())?;
    Ok((real, sshd))
}

/// Scenario: run a private `sshd` on an ephemeral loopback port with keys the
/// test generated, start two real `dot-agent-deck daemon serve` processes with
/// one stand-in agent each, and reach the first one through a real `ssh -N -L`
/// child forwarding a Unix socket while the second is observed locally. The
/// remote deck must complete its handshake and list its own agent over the
/// tunnel; the local trust check must be the one thing that does *not* run
/// against the forwarded socket; the two decks must mint different identities
/// and never share an agent; and dropping the connection must kill the `ssh`
/// child and remove the socket it bound.
#[spec("fleet/observe/002")]
#[test]
fn observe_002_a_remote_deck_is_reached_over_a_real_ssh_tunnel() {
    // Before `init_test_env`, and before anything spawns a thread: this is a
    // process-global write, and it has to be in force before the first read of
    // `XDG_RUNTIME_DIR` — `RemoteTunnel::open`'s, which decides where the
    // reaping sweep points.
    //
    // SAFETY: both e2e aliases run under nextest, which is process-per-test, so
    // this process is this test and nothing else is touching the environment.
    // It is also the first statement in the body, so no thread exists yet.
    let runtime_dir = common::harness_temp_root().join("rt");
    std::fs::create_dir_all(&runtime_dir).expect("create the sandbox XDG_RUNTIME_DIR");
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    }

    let (real_ssh, sshd_bin) = match preflight(&runtime_dir) {
        Ok(found) => found,
        Err(reason) => {
            skip_unless!(Err(reason));
            unreachable!()
        }
    };

    common::init_test_env();

    let sshd_dir = common::harness_temp_root().join("sshd");
    let sshd = match SandboxSshd::start(&sshd_dir, &sshd_bin) {
        Ok(sshd) => sshd,
        Err(reason) => {
            skip_unless!(Err(reason));
            unreachable!()
        }
    };
    let wrapper = sshd_dir.join("ssh-wrapper");
    write_ssh_wrapper(&wrapper, real_ssh.path(), &sshd.known_hosts)
        .expect("write the ssh wrapper, which is a file in this test's own temp dir");
    if let Err(reason) = sshd.smoke(&wrapper) {
        skip_unless!(Err(format!(
            "{reason} (sshd log: {})",
            std::fs::read_to_string(&sshd.log)
                .unwrap_or_default()
                .trim()
        )));
        unreachable!()
    }
    let ssh = SshProgram::at(&wrapper).expect("the wrapper path is absolute");

    // Two real daemon processes, as `fleet/observe/001` starts them. One will
    // be reached through the tunnel and one directly.
    let logs = common::harness_tempdir().expect("daemon log tempdir");
    let remote_log = logs
        .path()
        .join("remote.log")
        .to_string_lossy()
        .into_owned();
    let local_log = logs.path().join("local.log").to_string_lossy().into_owned();
    let remote_daemon =
        spawn_daemon_serve_with_env(None, "0", &[("DOT_AGENT_DECK_LOG", remote_log.as_str())]);
    let local_daemon =
        spawn_daemon_serve_with_env(None, "0", &[("DOT_AGENT_DECK_LOG", local_log.as_str())]);
    start_stand_in(&remote_daemon, REMOTE_AGENT, "pane-remote");
    start_stand_in(&local_daemon, LOCAL_AGENT, "pane-local");

    // The remote deck. `127.0.0.1` on the sandboxed sshd's port, the generated
    // key, and the far-end socket path — which on loopback is the same bytes
    // the daemon bound, and see this module's header for why that is a
    // limitation rather than a convenience.
    let remote_socket = RemoteSocketPath::parse(&remote_daemon.attach_socket.to_string_lossy())
        .expect(
            "the harness attach socket path fits sun_path — the preflight checked the longer one",
        );
    let remote = RemoteEndpoint::new(
        Hostname::parse("127.0.0.1").expect("a loopback literal is a valid host"),
        remote_socket,
    )
    .with_port(sshd.port)
    .with_key(KeyPath::parse(&sshd.client_key.to_string_lossy()).expect("the generated key path"));
    let remote_deck = Endpoint::Remote(remote);
    let local_deck = Endpoint::Local(LocalEndpoint::at(&local_daemon.attach_socket));

    // ------------------------------------------------------------------
    // (1) A remote deck is reachable at all.
    // ------------------------------------------------------------------
    let connection = EndpointConnection::open(&remote_deck, &ssh)
        .expect("the tunnel to the sandboxed sshd must come up");
    let EndpointConnection::Remote(tunnel) = &connection else {
        panic!("a remote endpoint must open a remote connection, not a local one");
    };
    let ssh_pid = tunnel.child_pid();
    let forwarded = connection.connect_address().to_path_buf();

    // The isolation claim, asserted rather than described: the socket this
    // tunnel bound — and therefore the directory `reap_orphaned_tunnels` just
    // swept — is inside this test's own runtime dir, not the operator's.
    assert!(
        forwarded.starts_with(&runtime_dir),
        "the forwarded socket must live under this test's XDG_RUNTIME_DIR, not \
         the operator's: {}",
        forwarded.display()
    );
    assert!(
        forwarded.exists(),
        "the local ssh client must have bound the forwarded socket at {}",
        forwarded.display()
    );
    assert_ne!(
        forwarded, remote_daemon.attach_socket,
        "every byte below must travel through the forwarded socket rather than \
         the daemon's own inode — if these were one path the transport would be \
         untested and the test would still be green"
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the remote-transport runtime");
    // `for_connection`, never `DaemonClient::new`: it is what carries
    // `EndpointPresence::Elsewhere` through, so nothing downstream may read a
    // `stat` of the forwarded socket as the daemon's health.
    let client_remote = DaemonClient::for_connection(&connection);
    let client_local =
        DaemonClient::for_endpoint(&local_deck).expect("a client for the local deck");
    assert_eq!(
        connection.presence(),
        EndpointPresence::Elsewhere,
        "a tunnelled deck's connect address exists right here, so only the \
         transport can say that a stat of it means nothing"
    );

    let caps = runtime
        .block_on(client_remote.capabilities())
        .expect("the remote daemon answers its handshake through the tunnel");
    assert!(
        caps.is_advertised(),
        "the daemon reached over ssh advertises a capability set like any other"
    );
    let listed_remote = runtime
        .block_on(client_remote.list_agents())
        .expect("the remote daemon lists its agents through the tunnel");
    assert_eq!(
        listed_remote.len(),
        1,
        "the remote deck carries its own agent"
    );
    assert_eq!(
        listed_remote[0].display_name.as_deref(),
        Some(REMOTE_AGENT),
        "and it is the agent that daemon started, arrived over a real forwarded \
         socket"
    );

    // ------------------------------------------------------------------
    // (2) The local trust check is correctly NOT run against the forwarded
    //     socket — and here is why that is a decision rather than an omission.
    // ------------------------------------------------------------------
    assert!(
        remote_deck.as_local().is_none(),
        "`as_local()` is the gate `daemon_bridge::establish` reads before it \
         runs `verify_endpoint_trusted`; a remote deck must not be able to \
         reach it"
    );
    // Run the check by hand on the very inode `establish` would have tested,
    // and watch it PASS. It passes because the *local* `ssh` client created
    // that socket, as us, at 0600 (`StreamLocalBindMask=0177`) — every
    // predicate it checks is a fact about our own ssh client and none of them
    // is a fact about the far end. A gate that cannot fail is not a gate, which
    // is exactly why `establish` does not run it here.
    dot_agent_deck::platform::fsperm::verify_endpoint_trusted(&forwarded).unwrap_or_else(
        |reason| {
            panic!(
                "the forwarded socket passes the local trust predicate — that is the \
             point of this assertion, so a failure means the predicate changed \
             meaning: {reason}"
            )
        },
    );

    // ------------------------------------------------------------------
    // (3) A remote and a local deck observed together: the fleet property,
    //     over a real tunnel.
    // ------------------------------------------------------------------
    let id_remote = remote_deck.identity().wire_id();
    let id_local = local_deck.identity().wire_id();
    assert_ne!(
        id_remote, id_local,
        "a tunnelled deck and a local deck must mint two identities — \
         `wire_id()` is the whole body of the desktop's `deck_wire_id()`"
    );
    let listed_local = runtime
        .block_on(client_local.list_agents())
        .expect("the local deck lists its agents");
    assert_eq!(
        listed_local.len(),
        1,
        "the local deck carries its own agent"
    );
    assert_eq!(listed_local[0].display_name.as_deref(), Some(LOCAL_AGENT));
    assert_eq!(
        listed_remote[0].id, listed_local[0].id,
        "two independent registries must really mint the same first id, or the \
         attribution above proves nothing"
    );
    assert_ne!(
        listed_remote[0].display_name, listed_local[0].display_name,
        "and no agent may appear on both decks"
    );

    // ------------------------------------------------------------------
    // (4) Teardown: dropping the last lease stops the ssh child.
    // ------------------------------------------------------------------
    assert!(
        common::process_running(ssh_pid as i32),
        "the ssh child must still be running while the connection is held"
    );
    drop(connection);
    // `process_running` reads `/proc` and treats a zombie as exited, so this
    // cannot be satisfied by an unreaped child — which is half of what the
    // assertion below is about.
    common::wait_until(TEARDOWN_TIMEOUT, || {
        !common::process_running(ssh_pid as i32)
    });
    assert!(
        !common::process_running(ssh_pid as i32),
        "`EndpointConnection`'s Drop must kill and reap the ssh child (pid \
         {ssh_pid}); an app that leaks one per reconnect is the orphan this \
         transport was built to avoid"
    );
    assert!(
        !forwarded.exists(),
        "and it must remove the socket it bound at {}",
        forwarded.display()
    );

    // The local deck is untouched by the remote deck's transport going away —
    // the fleet's per-deck degradation property, with a real child process as
    // the thing that died.
    let after_local = runtime
        .block_on(client_local.list_agents())
        .expect("the local deck keeps answering after the tunnel is torn down");
    assert_eq!(after_local.len(), 1);
    assert_eq!(after_local[0].display_name.as_deref(), Some(LOCAL_AGENT));

    drop(sshd);
}
