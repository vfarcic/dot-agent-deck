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
//! scrub for this side of the wall. Same safety argument as the harness's:
//! nextest gives every test its own process, so mutating this process's
//! environment cannot affect another test.
//!
//! **Necessary, not sufficient, and the difference matters.** Scrubbing THIS
//! process only stops a child from *inheriting* an endpoint. A child that emits
//! hook events resolves the endpoint itself when the variable is absent, and
//! [`crate::platform::paths::socket_path`]'s fallback is
//! `$XDG_RUNTIME_DIR/dot-agent-deck.sock` — the developer's live daemon. So an
//! unpinned emitter reaches a real deck whether or not the variable was
//! inherited, and `agent_pty::spawn`'s `env_remove` of the same four variables
//! cannot help either. Two things actually close that path, and a fixture that
//! spawns an emitter needs one of them:
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

/// Clear every inherited deck endpoint from this test process. Idempotent, and
/// safe to call from any unit test that spawns a pane or posts synthetic hook
/// events.
pub fn detach_from_any_live_deck() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let leaked: Vec<&str> = DECK_ENDPOINT_VARS
            .into_iter()
            .filter(|v| std::env::var_os(v).is_some())
            .collect();
        if !leaked.is_empty() {
            // Loud on purpose, matching the harness: the run is now safe, but
            // the contributor should know their shell was pointed at a live
            // deck.
            eprintln!(
                "note: detaching this test process from a live deck — cleared {}. \
                 The inherited values would have sent fixture hook events into \
                 your running dashboard.",
                leaked.join(", ")
            );
        }
        for var in leaked {
            // SAFETY: nextest runs one test per process and this is called from
            // the test body before it spawns anything, via a `OnceLock` so it
            // happens exactly once per process. Plain `cargo test` runs every
            // test as a thread of ONE process, where that argument does not
            // hold; there this writes only when the run inherited a variable
            // (it was started from inside a deck pane), so an ordinary run
            // never moves the environment under another test's thread.
            unsafe { std::env::remove_var(var) };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Marks the re-executed copy of this test binary that runs the detach
    /// with the variables already in its environment.
    const DETACH_CHILD: &str = "DAD_TEST_ISOLATION_DETACH_CHILD";

    /// Scenario: Start a fresh copy of this test binary with every deck
    /// identity variable set to a value that mimics a live deck, have it call
    /// the unit-test detach hook, and assert every one of them is gone — the
    /// `src/` half of `harness_clears_inherited_deck_endpoints`.
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
            for var in DECK_ENDPOINT_VARS {
                assert!(
                    std::env::var_os(var).is_some(),
                    "{var} was not inherited, so the detach below proves nothing"
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

    /// Scenario: Read every `.rs` file under `src/` and fail if any line that
    /// is not a comment installs a thread subscriber with
    /// `tracing::subscriber::set_default` outside this module — every capture
    /// has to go through [`capture_tracing_on_this_thread`] or it can lose
    /// events under plain `cargo test`.
    #[test]
    fn every_unit_test_capture_goes_through_the_seam() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(
            files.len() > 50,
            "the sweep found too few files to mean anything"
        );

        let needle = ["tracing::subscriber::", "set_default("].concat();
        let mut offenders = Vec::new();
        for file in files {
            if file.ends_with("test_isolation.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read source");
            for (n, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//") && line.contains(&needle) {
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
}
