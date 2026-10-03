//! Detach a **unit-test** process from any real deck — issue #666 follow-up.
//!
//! `tests/harness_isolation.rs` states the rule and `tests/common/mod.rs`
//! enforces it for the integration suite: running the tests from inside a deck
//! pane means this process inherits that pane's `DOT_AGENT_DECK_SOCKET` /
//! `_ATTACH_SOCKET` / `_PANE_ID` / `_AGENT_ID`, anything spawned inherits them
//! too, and its hooks then post into the developer's LIVE dashboard — a card
//! appears under a fixture pane id and vanishes again.
//!
//! That enforcement is `common::init_test_env()`, which lives under `tests/`.
//! The lib target's own `#[cfg(test)]` unit tests do not link `tests/common/`,
//! so nothing scrubbed the four variables for them. This module is that same
//! scrub for this side of the wall, and it is written the same way the
//! harness's is: only before `main`, never at run time (issue #678).
//!
//! **Scrubbing alone was necessary, not sufficient.** Scrubbing THIS process
//! only stops a child from *inheriting* an endpoint. With the variable absent,
//! both endpoint resolvers fall back to an address they compute for themselves —
//! [`crate::platform::paths::socket_path`]'s is
//! `$XDG_RUNTIME_DIR/dot-agent-deck.sock`, else
//! `${TMPDIR:-/tmp}/dot-agent-deck-<uid>/hook.sock`, and the client side then
//! also tries the legacy `/tmp/dot-agent-deck-<uid>.sock` spellings — and on a
//! developer's machine the first of those is the live daemon. Issue #1473
//! measured both kinds of leak with every variable scrubbed: a wrapped Codex
//! worker spawned by `orchestration/delegate/039` posted its fork-time
//! `SessionStart` there (a ghost "Codex" card that vanishes when selected), and
//! `dashboard/selection/016` — this process, not a child — asked the live
//! daemon for a close preview.
//!
//! **So the guard now also closes the fallback, and does it before `main`.**
//! [`detach_before_main`] runs as a constructor in every unit-test process:
//! it scrubs the variables above and points `XDG_RUNTIME_DIR` beneath
//! `/dev/null`, where nothing can exist ([`unreachable_runtime_dir`]). With
//! `XDG_RUNTIME_DIR` set, the resolvers take its arm and never consult the
//! `TMPDIR` or legacy `/tmp` rungs, so all three are closed at once — for this
//! process's own resolution and for every child that inherits its environment,
//! whether or not the test calls anything. Doing it before `main` is what
//! makes "a test written later" covered and also what makes the environment
//! write sound: no thread exists yet. The pins below remain for a child
//! started with a cleared environment, which inherits neither half.
//!
//! What it does not cover: a test that sets `XDG_RUNTIME_DIR` back, or removes
//! it, and then connects (it chose that address); a child spawned with
//! `env_clear` that is given neither a pin nor a runtime dir; and Windows,
//! where the default endpoints are per-user named pipes with no runtime-dir
//! rung to redirect.
//!
//! Two things close the remaining child path, for a fixture that spawns an
//! emitter with a cleared environment:
//!
//! * do not spawn a process that emits (a bare `/bin/cat` byte sink emits
//!   nothing — this is what `scheduler/dispatch/016` does since #666), or
//! * pin `DOT_AGENT_DECK_SOCKET` in the CHILD's environment at a path with no
//!   listener, so the emit fails closed instead of finding a stranger's daemon.
//!   [`pin_unreachable_endpoints`] (for a `SpawnOptions::env`) and
//!   [`unreachable_endpoints`] (for a `Command`'s `.envs(…)`) are that pin.
//!
//! **Enforced, narrowly, by linkage-check rule 17** (`unit-test-emitter-pins-endpoints`,
//! issue #688). A `fn` in `src/` test code that spawns an emitter the rule
//! recognises — a `SpawnOptions` literal declaring a Wrapper-strategy
//! `agent_type`, or a command literal naming a registered agent or the deck
//! binary — has to call one of those two helpers somewhere in its body. The
//! rule reads literals, so a command or type held in a variable walks past it;
//! `xtask/linkage-check/src/unit_test_endpoint_pin.rs` lists what else it
//! cannot see.

use std::sync::OnceLock;

/// The deck identity variables, in the order `tests/harness_isolation.rs` lists
/// them. Kept in step with `tests/common/mod.rs`'s `DECK_ENDPOINT_VARS`; the two
/// cannot share a constant because the lib target does not link that file.
///
/// Issue #1077 added `DOT_AGENT_DECK_PANE_CAPABILITY`. It is not an endpoint, but it
/// has the same failure mode as the pane and agent ids beside it, and a sharper
/// one: a test process started from inside a live deck pane inherits that pane's
/// real capability token, and a CLI it launches forwards it. Against the test's
/// own daemon that token was minted by a DIFFERENT daemon, so the message is
/// refused as `UnknownToken` — which, unlike a missing token, even
/// `DOT_AGENT_DECK_HOOK_PROVENANCE=warn` does not admit. Every test that stands
/// in for a pane by running the CLI would then fail only when run from a deck
/// pane, and pass everywhere else.
pub const DECK_ENDPOINT_VARS: [&str; 5] = [
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_PANE_ID",
    "DOT_AGENT_DECK_AGENT_ID",
    "DOT_AGENT_DECK_PANE_CAPABILITY",
];

/// Where [`detach_before_main`] points `XDG_RUNTIME_DIR`: a path beneath
/// `/dev/null`, which is a character device, so every lookup through it fails
/// with `ENOTDIR` — a connect, a bind, and a `create_dir_all` alike.
///
/// Not a directory under the temp dir, which is what the first cut of this fix
/// used (PR #1475 review). A predictable name there can be planted by another
/// local user as a symlink to the developer's real runtime dir, and an endpoint
/// trust check `lstat`s the socket, not the directories above it, so the test
/// would follow that link straight back to the live deck. Nobody can create an
/// entry beneath `/dev/null`, so there is nothing to plant, nothing to create
/// and nothing left behind. A test that needs a real runtime dir sets its own.
#[cfg(unix)]
fn unreachable_runtime_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/dev/null/dot-agent-deck-test-no-live-deck")
}

/// The identity variables [`detach_before_main`] found set, so the note in
/// [`detach_from_any_live_deck`] can still name them.
static CLEARED_BEFORE_MAIN: OnceLock<Vec<&'static str>> = OnceLock::new();

ctor::declarative::ctor! {
    /// Detach every unit-test process from any live deck before `main` — issue
    /// #1473. See the module docs for why this cannot wait for a test to call
    /// [`detach_from_any_live_deck`].
    ///
    /// Prints nothing: stderr is not promised to be usable before `main`.
    #[ctor(unsafe)]
    fn detach_before_main() {
        let leaked: Vec<&'static str> = DECK_ENDPOINT_VARS
            .into_iter()
            .filter(|v| std::env::var_os(v).is_some())
            .collect();
        // SAFETY: a constructor runs before `main`, so before libtest or any
        // code in this binary has started a thread, and nothing can observe
        // the environment mid-write. The only place this module writes the
        // environment:
        // `detach_from_any_live_deck` checks and writes nothing (issue #678).
        unsafe {
            for var in DECK_ENDPOINT_VARS {
                std::env::remove_var(var);
            }
            #[cfg(unix)]
            std::env::set_var("XDG_RUNTIME_DIR", unreachable_runtime_dir());
        }
        let _ = CLEARED_BEFORE_MAIN.set(leaked);
    }
}

/// Report what [`detach_before_main`] cleared. Idempotent, and safe to call
/// from any unit test that spawns a pane or posts synthetic hook events.
/// Writes nothing.
///
/// [`detach_before_main`] has already cleared the inherited variables — and
/// redirected `XDG_RUNTIME_DIR` — before the test began, so this only prints
/// the note naming what it cleared. It used to re-scrub a variable set again at
/// run time; that write raced any thread already reading the environment, which
/// the SAFETY argument for it ("nextest runs one test per process") did not
/// exclude — one process per test is not one thread per process (issue #678).
///
/// Unlike the integration harness's copy, it does not refuse a variable set
/// again in-process. Unit tests in this crate set endpoint variables in their
/// own process on purpose, under their own locks (`platform::paths`'s
/// precedence tests, `agent_pty`'s stale-socket scrub test, `config`'s
/// endpoint guard), and under plain `cargo test` those share a process with
/// every caller of this function — so a refusal here would fail an unrelated
/// test on a sibling's temporary value. A child spawned through
/// `agent_pty::spawn` does not inherit such a value either way, because that
/// path scrubs the named deck variables.
pub fn detach_from_any_live_deck() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let leaked: Vec<&str> = CLEARED_BEFORE_MAIN.get().cloned().unwrap_or_default();
        if !leaked.is_empty() {
            // Loud on purpose, matching the harness: the run is safe, but the
            // contributor should know their shell was pointed at a live deck.
            eprintln!(
                "note: detaching this test process from a live deck — cleared {}. \
                 The inherited values would have sent fixture hook events into \
                 your running dashboard.",
                leaked.join(", ")
            );
        }
    });
}

/// The two endpoint variables [`pin_unreachable_endpoints`] pins. Not the
/// pane/agent ids or the capability token: those are identity, not a route to
/// a daemon, and `agent_pty::spawn` already strips inherited copies of them.
const PINNED_ENDPOINT_VARS: [&str; 2] = ["DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"];

/// A per-process endpoint path nothing listens on, one per variable.
///
/// Never created: the point is that a `connect(2)` to it fails. The name
/// carries this process's pid so two concurrent test processes cannot collide
/// on a path one of them might later bind.
fn unreachable_endpoint(var: &str) -> String {
    let role = if var == "DOT_AGENT_DECK_ATTACH_SOCKET" {
        "attach"
    } else {
        "hook"
    };
    std::env::temp_dir()
        .join(format!(
            "dad-unit-no-listener-{}-{role}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

/// The hook and attach endpoints, pinned at paths with no listener, as
/// `(name, value)` pairs for a child's environment.
///
/// Why a pin rather than a scrub: with the variable ABSENT, a child that emits
/// resolves the endpoint itself — [`crate::platform::paths::socket_path`] falls
/// back to `$XDG_RUNTIME_DIR/dot-agent-deck.sock` when `XDG_RUNTIME_DIR` is set
/// — and on a developer's machine that is typically their live daemon. With it
/// PRESENT the resolver takes the override arm and never reaches the fallback,
/// so the emit fails closed. Issue #688 measured the difference: the scrub
/// alone still produced 3 foreign `SessionStart`s in 8 runs of one fixture.
/// Since issue #1473 [`detach_before_main`] redirects `XDG_RUNTIME_DIR` for
/// every child that inherits this process's environment; the pin is what still
/// holds for a child that does not.
///
/// For a `std::process::Command` / `tokio::process::Command`, pass this to
/// `.envs(…)`. For a `SpawnOptions`, use [`pin_unreachable_endpoints`], which
/// keeps any pin the caller already chose.
pub fn unreachable_endpoints() -> Vec<(String, String)> {
    PINNED_ENDPOINT_VARS
        .into_iter()
        .map(|var| (var.to_string(), unreachable_endpoint(var)))
        .collect()
}

/// `env` with [`unreachable_endpoints`] added for every endpoint variable it
/// does not already set. A value the caller supplied wins, so a fixture that
/// deliberately points its child at its OWN sandbox daemon keeps doing so.
///
/// Meant for `SpawnOptions::env`: `agent_pty::spawn` strips the inherited
/// endpoint variables and then applies `opts.env`, and
/// `AgentPtyRegistry::spawn_agent` injects the registry's own hook socket only
/// when `opts.env` names none — so a value placed here reaches the child.
///
/// Does not touch this process's environment; call
/// [`detach_from_any_live_deck`] for that half, before anything is spawned.
pub fn pin_unreachable_endpoints(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    for (var, value) in unreachable_endpoints() {
        if !env.iter().any(|(k, _)| *k == var) {
            env.push((var, value));
        }
    }
    env
}

/// Install `subscriber` as THIS thread's default for as long as the guard
/// lives. Every unit test that captures tracing output installs it through
/// here rather than with `tracing::subscriber::set_default` directly.
///
/// **Why a bare `set_default` loses events under plain `cargo test`.**
/// tracing-core caches each callsite's interest the first time any thread hits
/// it, and while exactly one dispatcher is registered it computes that interest
/// from the registering thread's OWN default rather than from every live
/// dispatcher. So with a test's capture subscriber the only one registered, a
/// different test's thread — one with no subscriber — that reaches a log site
/// first caches "never" for it, and the capturing test's own event at that
/// site is then dropped before its subscriber is asked. nextest never sees it,
/// because every test has its own process and so its own callsite cache.
/// Measured on `ingest_005_…`: the capture held every escaped site but one,
/// `Received event`, which other daemon tests reach without a subscriber.
///
/// The remedy is to make sure the one-dispatcher case never applies while a
/// capture is live: a second dispatcher is registered once, for the life of
/// the process, before any capture is installed. With two live, every callsite
/// registration combines all of them, the capture included. The extra one is
/// a [`tracing::subscriber::NoSubscriber`], which is no thread's default, so it
/// receives nothing and changes nothing a test observes.
pub fn capture_tracing_on_this_thread<S>(subscriber: S) -> tracing::subscriber::DefaultGuard
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    static SECOND_DISPATCHER: OnceLock<tracing::Dispatch> = OnceLock::new();
    SECOND_DISPATCHER
        .get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    tracing::subscriber::set_default(subscriber)
}

/// Write `contents` to `path` — a script or stand-in binary a test is about to
/// execute — without THIS process ever holding a write descriptor on it.
///
/// **Why `std::fs::write` is not enough under plain `cargo test`.** `execve`
/// refuses a file that any process has open for writing, with `ETXTBSY` ("Text
/// file busy"). `std::fs::write` holds such a descriptor for a moment, and if
/// another test's thread forks in that moment, the child inherits a copy. It
/// is close-on-exec, but the child keeps it until it gets to its own `exec`,
/// so an `exec` of the script that lands in between fails although this
/// process closed its descriptor long before. Measured on 2026-10-01: two
/// `remote_tunnel` tests failed that way in 15 runs, each with its own `ssh`
/// stand-in. nextest makes it rare rather than impossible, since a test that
/// spawns threads of its own can fork in that window too.
///
/// So the bytes go through a `/bin/cat` child, and the only descriptor ever
/// open for writing on the file is that child's own, gone when it exits.
/// Sets no mode: chmod the result as before. Elsewhere than Unix there is no
/// such refusal, and this is `std::fs::write`.
pub fn write_script(
    path: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exec /bin/cat > \"$1\"", "sh"])
            .arg(path.as_ref())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let written = child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(contents.as_ref());
        let output = child.wait_with_output()?;
        // The child's own failure first: when `cat` could not open the file
        // it exits without reading, and the write above then fails with a
        // broken pipe that says nothing about why.
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "writing {} through /bin/cat failed ({}): {}",
                path.as_ref().display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        written
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Marks the re-executed copy of this test binary that runs the detach
    /// with the variables already in its environment.
    const DETACH_CHILD: &str = "DAD_TEST_ISOLATION_DETACH_CHILD";

    /// Scenario: Start a fresh copy of this test binary with every deck
    /// identity variable set to a value that mimics a live deck, assert every
    /// one of them is already gone when the test body starts — cleared before
    /// `main` — and recorded as inherited, then have it call the unit-test
    /// detach hook and assert it names them in its note — the `src/` half of
    /// `harness_clears_inherited_deck_endpoints`.
    ///
    /// The variables go into a CHILD's environment rather than this one's.
    /// Under plain `cargo test` every test is a thread of one process, so a
    /// `set_var` here would point every concurrently running test at a
    /// pretend deck, and the `OnceLock` inside the detach has usually already
    /// fired for an earlier test, which made this fail on every such run.
    /// Setting them before the child starts is also the honest shape of the
    /// condition: a test process inherits them, it does not acquire them.
    #[test]
    fn detach_clears_inherited_deck_endpoints() {
        if std::env::var_os(DETACH_CHILD).is_some() {
            // Issue #1473: `detach_before_main` has already run, so the proof
            // that they were inherited is its record, not the environment.
            let cleared = CLEARED_BEFORE_MAIN.get().cloned().unwrap_or_default();
            for var in DECK_ENDPOINT_VARS {
                assert!(
                    cleared.contains(&var),
                    "{var} was not inherited, so the detach below proves nothing \
                     (cleared before main: {cleared:?})"
                );
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} reached the test body — the before-main detach did not clear it"
                );
            }

            detach_from_any_live_deck();

            for var in DECK_ENDPOINT_VARS {
                assert!(
                    std::env::var_os(var).is_none(),
                    "{var} survived the unit-test detach — a spawned child would \
                     inherit it and could post hook events into a live deck"
                );
            }
            return;
        }

        let name = "test_isolation::tests::detach_clears_inherited_deck_endpoints";
        let output = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(DETACH_CHILD, "1")
        .envs(
            DECK_ENDPOINT_VARS
                .into_iter()
                .map(|var| (var, "/run/user/1000/pretend-live-deck")),
        )
        .output()
        .expect("re-execute the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the detach left a deck variable behind:\n{stdout}\n{stderr}"
        );
        // libtest exits 0 when the filter matches nothing, so prove the child
        // actually ran the detach half.
        assert!(
            stdout.contains("1 passed"),
            "the child did not run {name}:\n{stdout}\n{stderr}"
        );
        assert!(
            stderr.contains("note: detaching this test process from a live deck"),
            "the child must report what it cleared:\n{stderr}"
        );
    }

    /// Scenario: Pin the endpoints onto an env that already names the pane id
    /// and a caller-chosen hook socket, and assert the pane id and the
    /// caller's socket survive untouched while the attach socket is added at a
    /// per-process path with no listener — so a child fails closed instead of
    /// resolving the developer's live daemon.
    #[test]
    fn pin_adds_missing_endpoints_and_keeps_the_callers_own() {
        let env = pin_unreachable_endpoints(vec![
            ("DOT_AGENT_DECK_PANE_ID".into(), "pane-1".into()),
            ("DOT_AGENT_DECK_SOCKET".into(), "/sandbox/own.sock".into()),
        ]);
        let get = |k: &str| {
            env.iter()
                .filter(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("DOT_AGENT_DECK_PANE_ID"), ["pane-1"]);
        assert_eq!(
            get("DOT_AGENT_DECK_SOCKET"),
            ["/sandbox/own.sock"],
            "a pin the caller chose must win, and must not be duplicated"
        );
        let attach = get("DOT_AGENT_DECK_ATTACH_SOCKET");
        assert_eq!(attach.len(), 1, "the missing attach pin must be added once");
        assert!(
            attach[0].contains(&format!("dad-unit-no-listener-{}-", std::process::id())),
            "the pin must be this process's no-listener path, got {}",
            attach[0]
        );
        assert!(
            !std::path::Path::new(attach[0]).exists(),
            "the pinned endpoint must not exist, or a connect could succeed"
        );
    }

    /// Set only on the re-executed child of
    /// [`a_unit_test_cannot_reach_a_live_deck_through_the_endpoint_fallback`];
    /// its value is the control endpoint.
    #[cfg(unix)]
    const PROBE_CONTROL_ENV: &str = "DAD_UNIT_LIVE_DECK_PROBE_CONTROL";

    /// A listening Unix socket standing in for a daemon, owner-only in an
    /// owner-only directory the way the daemon makes its own, so a client's
    /// trust check cannot be the reason nothing arrived.
    #[cfg(unix)]
    fn stand_in(path: &std::path::Path) -> std::os::unix::net::UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let dir = path.parent().expect("endpoint has a parent");
        std::fs::create_dir_all(dir).expect("create stand-in endpoint dir");
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod stand-in endpoint dir");
        let listener = std::os::unix::net::UnixListener::bind(path)
            .unwrap_or_else(|e| panic!("bind stand-in {}: {e}", path.display()));
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod stand-in endpoint");
        listener
            .set_nonblocking(true)
            .expect("nonblocking stand-in");
        listener
    }

    /// Everything that connected to `listener`, one string per connection.
    #[cfg(unix)]
    fn drain(listener: &std::os::unix::net::UnixListener) -> Vec<String> {
        use std::io::Read;
        let mut seen = Vec::new();
        while let Ok((mut stream, _)) = listener.accept() {
            stream.set_nonblocking(false).ok();
            stream
                .set_read_timeout(Some(std::time::Duration::from_millis(500)))
                .ok();
            let mut buf = Vec::new();
            let _ = stream.read_to_end(&mut buf);
            seen.push(String::from_utf8_lossy(&buf).into_owned());
        }
        seen
    }

    /// Scenario: Stand a fake "live deck" up at `$XDG_RUNTIME_DIR/dot-agent-deck.sock`
    /// and `…-attach.sock`, start a unit-test process that inherits that runtime
    /// dir but none of the deck endpoint variables and calls no guard, and have
    /// it resolve and connect to both endpoints the way
    /// `dashboard/selection/016`'s close preview did (issue #1473). A write to a
    /// control socket proves the child ran; nothing may reach the fake live deck.
    /// Then the same with `XDG_RUNTIME_DIR` unset and the fake deck under
    /// `$TMPDIR/dot-agent-deck-<uid>/`.
    #[cfg(unix)]
    #[test]
    fn a_unit_test_cannot_reach_a_live_deck_through_the_endpoint_fallback() {
        for runtime_dir_set in [true, false] {
            let root = crate::test_temp::tempdir().expect("disk-backed scratch dir");
            let live_dir = if runtime_dir_set {
                root.path().join("run")
            } else {
                root.path().join("tmp").join(format!(
                    "dot-agent-deck-{}",
                    crate::platform::paths::current_uid()
                ))
            };
            let (hook_name, attach_name) = if runtime_dir_set {
                ("dot-agent-deck.sock", "dot-agent-deck-attach.sock")
            } else {
                ("hook.sock", "attach.sock")
            };
            let hook = stand_in(&live_dir.join(hook_name));
            let attach = stand_in(&live_dir.join(attach_name));
            let control_path = root.path().join("control").join("probe.sock");
            let control = stand_in(&control_path);

            let mut child =
                std::process::Command::new(std::env::current_exe().expect("unit-test binary path"));
            child
                .args([
                    "--exact",
                    "test_isolation::tests::reexec_child_resolves_the_deck_endpoints",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(PROBE_CONTROL_ENV, &control_path)
                // What the existing detach already clears — #1473 is the half
                // left once these are gone, so start from there.
                .env_remove("DOT_AGENT_DECK_SOCKET")
                .env_remove("DOT_AGENT_DECK_ATTACH_SOCKET")
                .env_remove("DOT_AGENT_DECK_PANE_CAPABILITY");
            if runtime_dir_set {
                child.env("XDG_RUNTIME_DIR", root.path().join("run"));
            } else {
                child
                    .env_remove("XDG_RUNTIME_DIR")
                    .env("TMPDIR", root.path().join("tmp"));
            }
            let output = child.output().expect("re-execute the probe child");
            let child_log = format!(
                "child status {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success(),
                "the probe child failed\n{child_log}"
            );
            assert!(
                !drain(&control).is_empty(),
                "control: the probe child never reached its control socket, so it did not run \
                 the probe and the absence below would prove nothing\n{child_log}"
            );
            let leaked_hook = drain(&hook);
            let leaked_attach = drain(&attach);
            assert!(
                leaked_hook.is_empty() && leaked_attach.is_empty(),
                "a unit-test process reached the stand-in LIVE deck at {} through the endpoint \
                 fallback (issue #1473; XDG_RUNTIME_DIR set: {runtime_dir_set}) — on a \
                 developer's machine that is their running daemon.\n  hook received \
                 {leaked_hook:?}\n  attach received {leaked_attach:?}",
                live_dir.display()
            );
        }
    }

    /// The re-executed half of
    /// [`a_unit_test_cannot_reach_a_live_deck_through_the_endpoint_fallback`]; a
    /// no-op unless that test started this process.
    ///
    /// Scenario: Only when re-executed by the parent above, and without calling
    /// any isolation guard: connect to the hook and attach endpoints the client
    /// resolvers return, as `dashboard/selection/016`'s close preview did, then
    /// write to the parent's control socket to prove the probe ran.
    #[cfg(unix)]
    #[test]
    fn reexec_child_resolves_the_deck_endpoints() {
        use std::io::Write;
        let Some(control) = std::env::var_os(PROBE_CONTROL_ENV) else {
            return;
        };
        for endpoint in [
            crate::endpoint_resolve::client_socket_path(),
            crate::endpoint_resolve::client_attach_socket_path(),
        ] {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&endpoint) {
                let _ = writeln!(stream, "in-process probe from {}", std::process::id());
            }
        }
        let mut stream =
            std::os::unix::net::UnixStream::connect(control).expect("reach the control socket");
        let _ = writeln!(stream, "probe ran");
    }

    /// Scenario: Ask for the bare pins and assert both endpoint variables are
    /// present, distinct, and not the fallback a child would otherwise resolve.
    #[test]
    fn unreachable_endpoints_pin_both_routes_away_from_the_fallback() {
        let pins = unreachable_endpoints();
        let names: Vec<&str> = pins.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, PINNED_ENDPOINT_VARS);
        assert_ne!(pins[0].1, pins[1].1, "hook and attach pins must differ");
        for (_, value) in &pins {
            assert!(
                !value.ends_with("/dot-agent-deck.sock"),
                "a pin must not be the default hook endpoint: {value}"
            );
        }
    }

    /// The writer a capture test hands its subscriber: every byte, in order.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// One log site both threads below reach, so they share its cached interest.
    fn reach_the_shared_log_site(who: &str) {
        tracing::info!(who, "capture race probe");
    }

    /// Scenario: Install a capture subscriber on one thread, let a second
    /// thread with no subscriber reach a log site first, then emit at that same
    /// site from the capturing thread, and assert the capture holds the event.
    ///
    /// Without the second dispatcher [`capture_tracing_on_this_thread`]
    /// registers, the subscriber-less thread caches "never" for the site and
    /// the capturing thread's event is dropped. That is deterministic in a
    /// process of its own, which is what nextest gives this test; under plain
    /// `cargo test` it depends on what the other tests have registered.
    #[test]
    fn a_capture_keeps_a_log_site_another_thread_reached_first() {
        let captured = CapturedLog::default();
        let (installed_tx, installed_rx) = std::sync::mpsc::channel::<()>();
        let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();

        let capturing = std::thread::spawn({
            let captured = captured.clone();
            move || {
                let _guard = capture_tracing_on_this_thread(
                    tracing_subscriber::fmt()
                        .with_writer(captured)
                        .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
                        .with_ansi(false)
                        .finish(),
                );
                installed_tx.send(()).unwrap();
                reached_rx.recv().unwrap();
                reach_the_shared_log_site("capturing");
            }
        });
        let bare = std::thread::spawn(move || {
            installed_rx.recv().unwrap();
            reach_the_shared_log_site("bare");
            reached_tx.send(()).unwrap();
        });
        bare.join().unwrap();
        capturing.join().unwrap();

        let raw = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(
            raw.contains("capture race probe") && raw.contains("capturing"),
            "the capturing thread's event was dropped because another thread \
             reached the site first: {raw:?}"
        );
        assert!(
            !raw.contains("\"bare\"") && !raw.contains("who=\"bare\""),
            "a capture is this thread's alone: {raw:?}"
        );
    }

    /// Every `.rs` file under this crate's `src/`, with its text.
    fn src_files() -> Vec<(std::path::PathBuf, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut paths = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut paths,
        );
        assert!(
            paths.len() > 50,
            "the sweep found too few files to mean anything"
        );
        paths
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path).expect("read source");
                (path, text)
            })
            .collect()
    }

    /// Scenario: Read every `.rs` file under `src/` and fail if any line that
    /// is not a comment names `set_default` or `with_default` from
    /// `tracing::subscriber` or `tracing::dispatcher` outside this module — a
    /// call or an import alike — since every capture
    /// has to go through [`capture_tracing_on_this_thread`] or it can lose
    /// events under plain `cargo test`.
    #[test]
    fn every_unit_test_capture_goes_through_the_seam() {
        // The path segments, not the full call, so an imported
        // `use tracing::subscriber::set_default;` and a `subscriber::set_default(`
        // reached through `use tracing::subscriber;` are caught as well, and the
        // callback form `with_default` beside it. A glob import followed by a
        // bare `set_default(` still walks past.
        let needles: Vec<String> = ["subscriber::", "dispatcher::"]
            .iter()
            .flat_map(|module| ["set_default", "with_default"].map(|verb| [module, verb].concat()))
            .collect();
        let mut offenders = Vec::new();
        for (file, text) in src_files() {
            if file.ends_with("test_isolation.rs") {
                continue;
            }
            for (n, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//")
                    && needles.iter().any(|needle| line.contains(needle.as_str()))
                {
                    offenders.push(format!("{}:{}", file.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "install a capture with crate::test_isolation::capture_tracing_on_this_thread, \
             not a bare set_default: {offenders:#?}"
        );
    }

    /// Scenario: Write a script through the helper — more bytes than a pipe
    /// buffer holds at once, with a NUL and invalid UTF-8 in a trailing
    /// comment — make it executable, run it, and assert it ran and the file
    /// holds exactly those bytes; then point it at a directory that does not
    /// exist and assert the failure is reported.
    #[cfg(unix)]
    #[test]
    fn write_script_writes_exactly_the_bytes_and_the_result_runs() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let script = root.path().join("stand-in");
        let mut body = b"#!/bin/sh\necho ran\nexit 0\n# ".to_vec();
        body.extend(std::iter::repeat_n(b'x', 200_000));
        body.extend_from_slice(b"\0\xff\n");
        write_script(&script, &body).expect("write the script");
        assert_eq!(std::fs::read(&script).expect("read back"), body);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let out = std::process::Command::new(&script)
            .output()
            .expect("run it");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stdout, b"ran\n");

        let err = write_script(root.path().join("missing").join("x"), &body)
            .expect_err("a directory that does not exist must be reported");
        assert!(err.to_string().contains("/bin/cat"), "{err}");
    }

    /// Scenario: Read every `.rs` file under `src/` and fail where a
    /// `std::fs::write` / `tokio::fs::write` of a path is followed, within the
    /// next 40 lines, by a `set_permissions` that gives the same path an
    /// owner-execute bit — a file written in this process and then executed,
    /// which is the `ETXTBSY` shape [`write_script`] exists for.
    ///
    /// It reads the first argument as written, so a path reached through a
    /// different variable, a mode set through `PermissionsExt::set_mode`, or a
    /// write further away walks past it: a tripwire for the common shape, not
    /// a proof that no other exists.
    #[test]
    fn scripts_a_test_executes_are_written_through_the_seam() {
        fn first_arg(rest: &str) -> String {
            rest.split([',', ')'])
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('&')
                .to_string()
        }
        let writes = [
            ["std::fs::", "write("].concat(),
            ["tokio::fs::", "write("].concat(),
        ];
        let chmod = ["fs::", "set_permissions("].concat();
        let mut offenders = Vec::new();
        for (file, text) in src_files() {
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let Some(rest) = writes
                    .iter()
                    .find_map(|w| line.find(w.as_str()).map(|at| &line[at + w.len()..]))
                else {
                    continue;
                };
                let target = if rest.trim().is_empty() {
                    first_arg(lines.get(i + 1).copied().unwrap_or(""))
                } else {
                    first_arg(rest)
                };
                if target.is_empty() {
                    continue;
                }
                let window = &lines[i + 1..lines.len().min(i + 41)];
                for (j, later) in window.iter().enumerate() {
                    if later.contains(&format!("create_dir(&{target})")) {
                        break;
                    }
                    if !later.contains(&chmod) {
                        continue;
                    }
                    let joined = window[j..window.len().min(j + 3)].join(" ");
                    let after = &joined[joined.find(&chmod).expect("present") + chmod.len()..];
                    let owner_exec = after
                        .split("from_mode(0o")
                        .nth(1)
                        .and_then(|mode| mode.chars().next())
                        .and_then(|digit| digit.to_digit(8))
                        .is_some_and(|digit| digit & 1 == 1);
                    if first_arg(after) == target && owner_exec {
                        offenders.push(format!("{}:{}", file.display(), i + 1));
                        break;
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "write a file this process will execute with \
             crate::test_isolation::write_script, not an in-process write: {offenders:#?}"
        );
    }
}
