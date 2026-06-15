//! PRD #77 — TUI testing harness (L2 slice).
//!
//! Spawns the production `dot-agent-deck` binary inside a `portable-pty`
//! PTY, parses its stdout through a `vt100` grid, and exposes a small
//! fluent surface so tests can wait on observable state without
//! sleeping. Decision 20 pins the PTY size + color env so the grid is
//! deterministic; Decisions 12 + 21 + 28 govern per-test isolation,
//! quiescence-based waits, and failure recordings.
//!
//! Intentionally compiled unconditionally so this single module can be
//! shared by every L2 test under the `e2e` feature. The harness uses
//! production deps only (`portable-pty`, `vt100`, `tempfile`, `libc`,
//! `serde_json`), all already in `Cargo.toml`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Decision 21: tunable harness constant for `wait_until_quiescent`.
/// 50 ms idle window — long enough that the dashboard's full repaint
/// has settled, short enough that test runtime stays bounded.
pub const QUIESCENT_IDLE_MS: u64 = 50;

/// Default ceiling on quiescence / signal waits. Tests do not pass a
/// budget — quiescence and string-signal waits are bounded internally.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Decision 20: pinned PTY dimensions for the deck. Resize tests
/// override via `TuiDeck::resize`.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// One byte-stream chunk recorded for the asciinema cast on failure
/// (Decision 28). Time is seconds since session start; data is the
/// raw bytes off the PTY master, which is what asciinema-format
/// `agg` and `asciinema play` expect.
#[derive(Debug, Clone)]
struct CastEvent {
    offset_secs: f64,
    data: Vec<u8>,
}

/// Optional pre-staged saved-session entry — when set, the harness
/// generates a `session.toml` under the per-test tempdir and passes
/// `--continue` so the deck auto-opens one pane running this command
/// at launch. Used by chain-smoke tests to drive real agents
/// (PRD #77 Decision 8) without user keystrokes.
#[derive(Debug, Clone)]
struct ContinueSession {
    pane_name: String,
    command: String,
}

/// Which agent's credential set the test wants imported from the
/// host HOME into the per-test tempdir HOME. M3.1 auditor Nit 1 —
/// the M2.1 N3 attribution was misleading: M2.1 banned symlinks in
/// the fixture-copy path, and M3.1 carries that ban forward into
/// the credential-copy path with a hard refuse (source symlink ->
/// Err) and atomic 0o600 creation on the destination (S2 + S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialImport {
    ClaudeCode,
    OpenCode,
}

/// Builder for [`TuiDeck`]. Use the test surface
/// [`TuiDeck::builder`].
pub struct TuiDeckBuilder {
    cols: u16,
    rows: u16,
    extra_env: Vec<(String, String)>,
    continue_session: Option<ContinueSession>,
    credential_imports: Vec<CredentialImport>,
    keybindings_toml: Option<String>,
}

impl TuiDeckBuilder {
    /// Override an environment variable for the spawned binary. Tests
    /// use this when their behaviour-under-test demands a different
    /// value than Decision 20's pinned default (e.g. `NO_COLOR=1`).
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Override the initial PTY size. Resize tests do this when the
    /// behaviour under test depends on a non-default geometry.
    pub fn with_pty_size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Stage a `session.toml` in the per-test tempdir and pass
    /// `--continue` on launch so the deck auto-opens one pane running
    /// `command` against the tempdir as its working directory.
    /// Used by chain-smoke tests to drive a real agent CLI without
    /// keystrokes.
    pub fn with_continue_session(
        mut self,
        pane_name: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        self.continue_session = Some(ContinueSession {
            pane_name: pane_name.into(),
            command: command.into(),
        });
        self
    }

    /// Import the host user's Claude Code credentials + settings into
    /// the per-test tempdir HOME so a spawned `claude` CLI can
    /// authenticate. Hook entries in the imported `settings.json` are
    /// stripped — the deck installs its own hooks pointing at the
    /// per-test paths. The destination credential file is created
    /// atomically with mode 0o600 (M3.1 auditor S2) and the source
    /// path is refused if it's a symlink (M3.1 auditor S3).
    ///
    /// The actual copy happens at launch time. Missing or
    /// unreadable credentials surface through
    /// [`try_launch_with_fixture`](Self::try_launch_with_fixture)
    /// as `Err(reason)`; the convenience
    /// [`launch_with_fixture`](Self::launch_with_fixture) panics
    /// instead. Pair with [`check_claude_available`] and
    /// [`skip_unless!`] to convert that into a clean
    /// Decision-26 runtime skip.
    pub fn with_imported_claude_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::ClaudeCode);
        self
    }

    /// Same shape as [`with_imported_claude_credentials`] but for
    /// OpenCode (`~/.opencode/`, `~/.config/opencode/opencode.jsonc`).
    /// The deck installs its own plugin into the tempdir HOME, so any
    /// `~/.opencode/plugin/` directory on the host is NOT copied.
    pub fn with_imported_opencode_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::OpenCode);
        self
    }

    /// Stage a `keybindings.toml` in the per-test HOME's config dir
    /// (`$HOME/.config/dot-agent-deck/keybindings.toml`, mirroring the
    /// `config.toml` path resolved by `dot_agent_deck::config`) before
    /// launch, so the deck reads it during startup. `content` is written
    /// verbatim — pass malformed TOML to exercise the fallback path
    /// (PRD #40 `keybindings/fallback/*`). The file is created with the
    /// HOME-relative path so two clients in the same suite never share
    /// bindings.
    pub fn with_keybindings_toml(mut self, content: impl Into<String>) -> Self {
        self.keybindings_toml = Some(content.into());
        self
    }

    /// Launch the deck against the named fixture under
    /// `tests/fixtures/`. The fixture is copied into the per-test
    /// tempdir at launch (Decision 12); the deck's `HOME`, hook socket,
    /// and attach socket all point inside that tempdir.
    ///
    /// Panics on credential-import / setup failure. For tests that
    /// would rather surface those errors as a `Result`, call
    /// [`try_launch_with_fixture`](Self::try_launch_with_fixture).
    pub fn launch_with_fixture(self, fixture_name: &str) -> TuiDeck {
        self.try_launch_with_fixture(fixture_name)
            .unwrap_or_else(|e| panic!("launch_with_fixture failed: {e}"))
    }

    /// Fallible variant of [`launch_with_fixture`]. Returns
    /// `Err(reason)` on credential-import or other setup failures
    /// where the reason is the same user-facing string the
    /// `check_*_available()` helpers produce (per Decision 26
    /// runtime-skip wording — M3.1 reviewer Nit 3).
    pub fn try_launch_with_fixture(self, fixture_name: &str) -> Result<TuiDeck, String> {
        TuiDeck::try_launch_inner(self, fixture_name)
    }
}

/// Handle to a running deck.
pub struct TuiDeck {
    pty_master: Box<dyn MasterPty + Send>,
    /// PTY master write side, taken ONCE at construction. `MasterPty::
    /// take_writer()` is single-shot (a 2nd call errors), so `send_keys` /
    /// `send_bytes` (and `click`/`scroll`, which call it 2×/1×) must share one
    /// stored writer rather than taking a fresh one per call. Behind a `Mutex`
    /// so the write helpers can keep `&self`.
    writer: Mutex<Box<dyn Write + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_byte_at: Arc<Mutex<Instant>>,
    cast_events: Arc<Mutex<Vec<CastEvent>>>,
    /// M4.6 P1: append-only buffer of EVERY byte the reader thread
    /// has seen since launch. `wait_for_strings_in_order` snapshots
    /// this against an index captured at call time so two status
    /// transitions rendered in the same polling window can't race
    /// the wait past one of them — the substring search runs over
    /// the rolling history, not the live vt100 grid. Bounded by
    /// total test duration (the harness's 10s wait ceiling +
    /// per-test cap) — same memory profile as `cast_events`.
    byte_history: Arc<Mutex<Vec<u8>>>,
    cast_started_at: Instant,
    reader_stop: Arc<AtomicBool>,
    reader_handle: Option<JoinHandle<()>>,
    child: Box<dyn Child + Send + Sync>,
    tempdir: tempfile::TempDir,
    home: PathBuf,
    hook_socket: PathBuf,
    attach_socket: PathBuf,
    fixture_path: PathBuf,
    test_name: String,
    cols: u16,
    rows: u16,
    record_on_success: bool,
}

impl TuiDeck {
    /// One-line convenience: build a default deck and launch it.
    pub fn launch_with_fixture(fixture_name: &str) -> Self {
        Self::builder().launch_with_fixture(fixture_name)
    }

    /// Start a fluent builder for non-default deck launches.
    pub fn builder() -> TuiDeckBuilder {
        TuiDeckBuilder {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            extra_env: Vec::new(),
            continue_session: None,
            credential_imports: Vec::new(),
            keybindings_toml: None,
        }
    }

    fn try_launch_inner(builder: TuiDeckBuilder, fixture_name: &str) -> Result<Self, String> {
        let test_name = current_test_name();

        // M2.1 auditor S1 + M3.1 auditor S4: create the per-test
        // tempdir with mode 0o700 atomically. `tempfile::tempdir()`
        // followed by `set_permissions(0o700)` had a small umask-derived
        // 0o755 window between creation and chmod — closed here by
        // asking tempfile to apply 0o700 at creation.
        let tempdir = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tempfile::Builder::new()
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir()
                    .expect("create per-test tempdir")
            }
            #[cfg(not(unix))]
            {
                tempfile::tempdir().expect("create per-test tempdir")
            }
        };
        let work = tempdir.path().to_path_buf();

        // Verify the atomic-creation 0o700 mode actually stuck —
        // catches a future tempfile API rename that would silently
        // skip the permission application.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&work)
                .expect("stat tempdir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "tempdir mode is 0o{mode:o}, expected 0o700 (M3.1 auditor S4 — atomic creation should have stamped this)"
            );
        }

        // Decision 12: copy fixture into the tempdir, then `git init`
        // (some deck paths probe `.git`).
        let fixture_src = locate_fixture(fixture_name);
        copy_dir_recursively(&fixture_src, &work).expect("copy fixture into tempdir");
        let _ = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(&work)
            .status();

        let home = work.join("home");
        std::fs::create_dir_all(&home).expect("create per-test HOME");

        // PRD #40: stage the keybindings config the deck reads at
        // startup. Path mirrors `config_path()` in
        // `dot_agent_deck::config` — `$HOME/.config/dot-agent-deck/` —
        // with the filename `keybindings.toml`. Written before the
        // binary spawns so the deck sees it on its first config read.
        if let Some(ref kb) = builder.keybindings_toml {
            let cfg_dir = home.join(".config").join("dot-agent-deck");
            std::fs::create_dir_all(&cfg_dir).expect("create keybindings config dir");
            std::fs::write(cfg_dir.join("keybindings.toml"), kb).expect("write keybindings.toml");
        }

        // Chain-smoke credential imports (PRD #77 Decision 8). Tests
        // pair these with `check_*_available()` + `skip_unless!`; if
        // the credentials disappeared between the precheck and here,
        // we surface a Decision-26-shaped error through `try_launch_*`
        // (M3.1 reviewer Nit 3) so the test's harness frame doesn't
        // panic mid-suite — callers can choose whether to skip or
        // bubble up.
        for kind in &builder.credential_imports {
            match kind {
                CredentialImport::ClaudeCode => {
                    import_claude_credentials(&home).map_err(|e| e.to_string())?;
                }
                CredentialImport::OpenCode => {
                    import_opencode_credentials(&home).map_err(|e| e.to_string())?;
                }
            }
        }

        // Write the saved-session file the deck reads under `--continue`,
        // if the test asked for one. The pane runs `command` in the
        // tempdir's working directory so the agent has a real cwd to
        // operate against (the deck's restore path skips panes whose
        // `dir` doesn't exist on disk).
        let session_toml_path = work.join("session.toml");
        if let Some(cs) = &builder.continue_session {
            write_continue_session_file(&session_toml_path, &work, &cs.pane_name, &cs.command)
                .expect("write continue session.toml");
        }

        let hook_socket = work.join("hook.sock");
        let attach_socket = work.join("attach.sock");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: builder.rows,
                cols: builder.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // Cargo sets `CARGO_BIN_EXE_<bin-name>` at integration-test
        // build time to the path of the freshly-built binary under
        // test. The `env!()` evaluates at compile time so the harness
        // always launches whatever the current test build produced
        // (debug vs. release matches the test's profile).
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = CommandBuilder::new(bin);
        cmd.cwd(&work);
        // Pass `--continue` when a saved session was staged so the deck
        // auto-opens the chain-smoke pane on launch.
        if builder.continue_session.is_some() {
            cmd.arg("--continue");
        }
        // M2.1 auditor S2: portable-pty 0.8 unconditionally env_clears
        // on Unix before applying our `cmd.env(...)` calls, but the old
        // comment claimed env_clear was avoided. Make the scrub
        // explicit so the behavior is documented in this file and not
        // dependent on an internal portable-pty detail.
        cmd.env_clear();

        // Decision 20: pinned env values. Order: portable-pty env_clear
        // above means nothing leaks from the host; we then set Decision
        // 20's pins, and finally layer the test's `with_env` overrides
        // (so a test asking for `NO_COLOR=1` still wins).
        let state_dir = work.join("state");
        let pinned: &[(&str, &str)] = &[
            ("TERM", "xterm-256color"),
            ("LC_ALL", "C.UTF-8"),
            ("COLORTERM", "truecolor"),
            // M2.1 auditor S3: pin SHELL so portable-pty cannot leak
            // the parent password DB entry on Unix. /bin/sh is
            // sufficient for the deck's spawn paths.
            ("SHELL", "/bin/sh"),
            ("HOME", home.to_str().expect("HOME path is UTF-8")),
            (
                "DOT_AGENT_DECK_SOCKET",
                hook_socket.to_str().expect("hook sock path is UTF-8"),
            ),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                attach_socket.to_str().expect("attach sock path is UTF-8"),
            ),
            // PRD #93 lazy-spawn writes a per-user lock dir. Pin it to
            // the tempdir so concurrent tests do not race on
            // `~/.cache/dot-agent-deck/spawn.lock`.
            (
                "DOT_AGENT_DECK_STATE_DIR",
                state_dir.to_str().expect("state dir is UTF-8"),
            ),
            // Disable the idle-shutdown so the daemon does not race the
            // test by exiting after a brief detach.
            ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0"),
            // Leaked-daemon safety net: the deck lazy-spawns its daemon
            // DETACHED (its own session → parent is PID 1 from birth), so the
            // orphan watchdog can't be enabled here (it would fire instantly).
            // The max-lifetime backstop is the right net for a detached daemon:
            // even if a test is SIGKILL'd / panics / times out before `Drop`
            // runs, the inherited cap makes the daemon self-exit gracefully
            // within 300s instead of leaking to PID 1 for hours/days. Idle
            // shutdown stays disabled (above) for determinism.
            ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300"),
        ];
        // PATH is required for the deck to spawn its own daemon
        // subcommand (it shells out via `current_exe`, but lookups like
        // git still need PATH).
        let inherit_pass = ["PATH"];

        let mut final_env: HashMap<String, String> = HashMap::new();
        for k in inherit_pass {
            if let Ok(v) = std::env::var(k) {
                final_env.insert(k.into(), v);
            }
        }
        for (k, v) in pinned {
            final_env.insert((*k).into(), (*v).into());
        }
        // Point the deck's saved-session reader at our staged file so
        // `--continue` picks up exactly the chain-smoke pane and
        // nothing from the developer's real session.toml.
        if builder.continue_session.is_some() {
            final_env.insert(
                "DOT_AGENT_DECK_SESSION".into(),
                session_toml_path
                    .to_str()
                    .expect("session.toml path is UTF-8")
                    .to_string(),
            );
        }
        // Decision 20: NO_COLOR and CLICOLOR_FORCE must NOT leak in.
        // We set up `final_env` from scratch, so they are absent by
        // construction — the only path back in is the test's own
        // `with_env` override (which we honour).
        for (k, v) in builder.extra_env {
            final_env.insert(k, v);
        }
        for (k, v) in final_env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn dot-agent-deck");
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            builder.rows,
            builder.cols,
            0,
        )));
        let last_byte_at = Arc::new(Mutex::new(Instant::now()));
        let cast_events = Arc::new(Mutex::new(Vec::<CastEvent>::new()));
        let byte_history = Arc::new(Mutex::new(Vec::<u8>::new()));
        let reader_stop = Arc::new(AtomicBool::new(false));
        let cast_started_at = Instant::now();

        // Reader thread: pulls bytes off the PTY master, feeds the
        // parser, updates `last_byte_at`, and appends to the cast log
        // plus the byte-history buffer (M4.6 P1, for race-free
        // `wait_for_strings_in_order`).
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let parser_for_reader = Arc::clone(&parser);
        let last_for_reader = Arc::clone(&last_byte_at);
        let cast_for_reader = Arc::clone(&cast_events);
        let history_for_reader = Arc::clone(&byte_history);
        let stop_for_reader = Arc::clone(&reader_stop);
        let start_for_reader = cast_started_at;
        let reader_handle = std::thread::Builder::new()
            .name(format!("tui-deck-reader-{test_name}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while !stop_for_reader.load(Ordering::Relaxed) {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            parser_for_reader.lock().unwrap().process(chunk);
                            *last_for_reader.lock().unwrap() = Instant::now();
                            cast_for_reader.lock().unwrap().push(CastEvent {
                                offset_secs: start_for_reader.elapsed().as_secs_f64(),
                                data: chunk.to_vec(),
                            });
                            history_for_reader.lock().unwrap().extend_from_slice(chunk);
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::Interrupted
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn reader thread");

        let record_on_success = std::env::var_os("DOT_AGENT_DECK_RECORD").is_some();

        // Take the PTY write side exactly once — `take_writer()` is
        // single-shot, so the per-call `take_writer()` the write helpers used
        // before panicked on their 2nd invocation (and dropped/closed the
        // write side after the 1st). Store it for all writes.
        let writer = pair.master.take_writer().expect("take PTY master writer");

        Ok(TuiDeck {
            pty_master: pair.master,
            writer: Mutex::new(writer),
            parser,
            last_byte_at,
            cast_events,
            byte_history,
            cast_started_at,
            reader_stop,
            reader_handle: Some(reader_handle),
            child,
            tempdir,
            home,
            hook_socket,
            attach_socket,
            fixture_path: work,
            test_name,
            cols: builder.cols,
            rows: builder.rows,
            record_on_success,
        })
    }

    /// Resize the PTY mid-run. Exercises the SIGWINCH path covered by
    /// the resize/* catalog area.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.pty_master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize PTY");
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Quiescence wait: blocks until the deck has emitted no bytes for
    /// at least [`QUIESCENT_IDLE_MS`].
    pub fn wait_until_quiescent(&self) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let idle = Duration::from_millis(QUIESCENT_IDLE_MS);
        loop {
            let since = {
                let last = *self.last_byte_at.lock().unwrap();
                Instant::now().duration_since(last)
            };
            if since >= idle {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "deck did not become quiescent within {WAIT_TIMEOUT:?} \
                     (idle window {QUIESCENT_IDLE_MS}ms)"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Deterministic wait until `pred` holds for the current rendered grid
    /// (one string, rows joined by '\n'), or panic after the timeout. Unlike
    /// [`wait_until_quiescent`], this does not depend on the PTY going idle —
    /// with a live daemon event stream the deck redraws often enough that a
    /// 50 ms idle window may never occur, so quiescence is unreliable for
    /// mouse specs. Use this to wait for a specific observable outcome (e.g.
    /// a row gaining the selection marker, or a modal/form closing) after a
    /// click or keystroke.
    pub fn wait_until_grid(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            let grid = self.snapshot_grid();
            if pred(&grid) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "did not reach grid state {what:?} within {WAIT_TIMEOUT:?}.\nFinal grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `needle` is ABSENT from the rendered grid, or panic after
    /// the timeout. For asserting a modal/overlay/form closed.
    pub fn wait_for_absence(&self, needle: &str) {
        self.wait_until_grid(&format!("absence of {needle:?}"), |g| !g.contains(needle));
    }

    /// Opt-in fast wait when the test knows the screen contents it is
    /// looking for. Decision 21: use sparingly.
    pub fn wait_for_string(&self, needle: &str) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            {
                let parser = self.parser.lock().unwrap();
                if parser.screen().contents().contains(needle) {
                    return;
                }
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                panic!(
                    "did not see {needle:?} within {WAIT_TIMEOUT:?}.\n\
                     Final grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `needle` appears anywhere in the deck's cumulative
    /// byte stream since launch — including bytes emitted *before* this
    /// call. Unlike [`wait_for_string`] (which only sees the current
    /// vt100 grid) and [`wait_for_strings_in_order`] (which only
    /// considers bytes after the call), this scans the entire rolling
    /// history from offset 0. Used to assert on transient output the
    /// deck prints before taking over the alternate screen — e.g. a
    /// startup warning written to stderr (which, on a PTY, is merged
    /// into the same byte stream as stdout) before the TUI clears the
    /// screen. The warning text scrolls out of the visible grid but
    /// stays in the byte history, so this is the only primitive that
    /// can observe it.
    pub fn wait_for_stream_string(&self, needle: &str) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            {
                let hist = self.byte_history.lock().unwrap();
                let text = String::from_utf8_lossy(&hist);
                if text.contains(needle) {
                    return;
                }
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                panic!(
                    "did not see {needle:?} anywhere in the byte stream within \
                     {WAIT_TIMEOUT:?}.\nFinal grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for `needles` to appear, in order, in the cumulative
    /// byte stream the deck has emitted since this call started.
    ///
    /// Unlike [`wait_for_string`], which asserts against the *current*
    /// rendered grid, this primitive walks a rolling history of every
    /// byte the PTY reader thread has captured. Two transitions
    /// rendered in the same ~20 ms polling window (e.g. Thinking →
    /// Working on a fast Haiku response) both land in the history,
    /// so a later poll still finds the earlier substring rather than
    /// spinning past it (M4.6 P1 / Decision 9: flake = bug).
    ///
    /// Semantics:
    /// - History is snapshotted from the byte-history buffer at call
    ///   time; bytes the deck emitted before this call are NOT
    ///   considered.
    /// - Each substring must be observed AFTER its predecessor was
    ///   observed (strictly increasing offsets).
    /// - Single 10-second total ceiling — internal poll cadence is
    ///   ~20 ms.
    /// - Substrings are matched against a lossy UTF-8 decode of the
    ///   raw bytes; status labels like `Thinking` / `Working` / `Bash`
    ///   / `Idle` are plain ASCII and unaffected by interleaved ANSI
    ///   control sequences.
    pub fn wait_for_strings_in_order(&self, needles: &[&str]) {
        if needles.is_empty() {
            return;
        }
        let start_idx = self.byte_history.lock().unwrap().len();
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            let snapshot: Vec<u8> = {
                let hist = self.byte_history.lock().unwrap();
                if hist.len() > start_idx {
                    hist[start_idx..].to_vec()
                } else {
                    Vec::new()
                }
            };
            let matched = match_needles_in_order(&snapshot, needles);
            if matched == needles.len() {
                return;
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                let so_far = needles[..matched].join(", ");
                let next = needles[matched];
                panic!(
                    "did not see `{next}` (needle #{} of {} — already \
                     matched in order: [{so_far}]) within {WAIT_TIMEOUT:?}.\n\
                     Final grid:\n{grid}",
                    matched + 1,
                    needles.len(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send raw bytes to the deck as if typed at the terminal. Writes
    /// to the PTY master so the spawned binary reads them on stdin and
    /// `crossterm` decodes them into key events. Callers pass the
    /// terminal byte encoding of the keypress — e.g. `b"\x03"` for
    /// Ctrl+C, `b"\x0e"` for Ctrl+n, `b"?"` for a literal `?`,
    /// `b"\x1bOP"` for F1, or an ESC-prefixed sequence like `b"\x1bL"`
    /// for Alt+Shift+L. The whole slice is written in one syscall so a
    /// multi-byte sequence is decoded as a single chord, not as
    /// separate keys.
    pub fn send_keys(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).expect("write keys to PTY master");
        writer.flush().expect("flush keys to PTY master");
    }

    /// Returns the deck's per-test hook socket path. Synthetic-event
    /// L2 tests connect to this directly to inject hook payloads.
    pub fn hook_socket_path(&self) -> &Path {
        &self.hook_socket
    }

    /// Returns the deck's per-test attach socket path.
    pub fn attach_socket_path(&self) -> &Path {
        &self.attach_socket
    }

    /// The deck's working directory (the copied fixture root, and the deck's
    /// cwd). Tests use it to drop runtime files (agent scripts, record files)
    /// the spawned agent can reach via a cwd-relative path.
    pub fn workdir(&self) -> &Path {
        &self.fixture_path
    }

    /// Return the parsed grid contents — used by `wait_for_string`
    /// internally and by tests that want to assert on full-screen
    /// state.
    pub fn snapshot_grid(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Write raw bytes to the deck's PTY master — the input side of the
    /// terminal. Lets L2 tests drive the deck the way a user's keyboard or
    /// mouse would (key bytes, SGR mouse reports). Flushes so the deck sees
    /// the input promptly.
    pub fn send_bytes(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).expect("write to PTY master");
        writer.flush().expect("flush PTY master");
    }

    /// Send a left-button mouse click at the given 0-based grid cell
    /// (`col`, `row`) as an SGR (1006) extended mouse report — press then
    /// release — matching what crossterm's `EnableMouseCapture` makes the
    /// deck decode. SGR coordinates are 1-based, so each is offset by one.
    pub fn click(&self, col: u16, row: u16) {
        let cx = col + 1;
        let cy = row + 1;
        // \x1b[<0;cx;cyM = left-button press; trailing `m` = release.
        self.send_bytes(format!("\x1b[<0;{cx};{cy}M").as_bytes());
        self.send_bytes(format!("\x1b[<0;{cx};{cy}m").as_bytes());
    }

    /// Send a mouse wheel scroll at the given 0-based grid cell as an SGR
    /// (1006) report (button code 64 = wheel up, 65 = wheel down), matching
    /// what crossterm decodes to `MouseEventKind::ScrollUp`/`ScrollDown`.
    /// Lets tests assert that scroll events reach the scroll path rather than
    /// being intercepted by the button hit-test layer.
    pub fn scroll(&self, col: u16, row: u16, down: bool) {
        let cb = if down { 65 } else { 64 };
        let cx = col + 1;
        let cy = row + 1;
        self.send_bytes(format!("\x1b[<{cb};{cx};{cy}M").as_bytes());
    }

    /// Locate the first occurrence of `needle` in the current rendered
    /// grid, returning its 0-based `(col, row)` start cell, or `None` if it
    /// is not on screen. Used by click tests to find a button's on-screen
    /// position before clicking it (so the test follows the real layout
    /// rather than hard-coding coordinates).
    pub fn find_in_grid(&self, needle: &str) -> Option<(u16, u16)> {
        let grid = self.snapshot_grid();
        for (row, line) in grid.lines().enumerate() {
            if let Some(byte_idx) = line.find(needle) {
                let col = line[..byte_idx].chars().count();
                return Some((col as u16, row as u16));
            }
        }
        None
    }
}

impl Drop for TuiDeck {
    fn drop(&mut self) {
        // Decision 28: dump recordings when the test panicked (failure),
        // or unconditionally when `DOT_AGENT_DECK_RECORD=1` (developer
        // opt-in for capturing successful runs).
        let panicking = std::thread::panicking();
        let should_dump = panicking || self.record_on_success;

        // Stop the reader, then kill the child. Order matters: if we
        // kill first the reader sees EOF mid-buffer and the cast loses
        // its tail. Stop the reader instead so the partial buffer
        // already lives in `cast_events`.
        self.reader_stop.store(true, Ordering::Relaxed);
        // Reap the whole process tree, not just the deck itself. portable-pty
        // makes the spawned deck a session/process-group leader (pgid == pid),
        // so a negative-pid `kill` signals every non-detached descendant in its
        // group (best-effort; ignore errors). Then the normal child kill+wait
        // as the fallback. (The deck's own lazy-spawned daemon setsid's into a
        // separate session and escapes this group — its
        // `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` cap is the net for that.)
        if let Some(pid) = self.child.process_id() {
            // SAFETY: kill(2) with a negative pid signals the process group;
            // SIGKILL has no failure mode beyond ESRCH/EPERM, which we ignore.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();

        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }

        if should_dump {
            // M4.3 flattened layout: each test gets its own per-test
            // subdirectory under `.dot-agent-deck/recordings/`, so
            // the cast and any failure artifacts sit alongside the
            // generated `.md`. `.dot-agent-deck/` is gitignored, so
            // the dump is purely developer-machine state — like
            // `target/`. The per-run subdir from M2.1 is gone:
            // concurrent `cargo test-e2e` on the same checkout is
            // not a real-world workflow, and the per-test path means
            // a re-run simply replaces the previous artifacts.
            let recordings_dir =
                workspace_recordings_root().join(sanitize_test_name(&self.test_name));
            if let Err(e) = self.dump_recordings(&recordings_dir) {
                eprintln!("[tui-harness] failed to write recordings to {recordings_dir:?}: {e}");
            }
            // PRD #77 Decision 30 / M4: regenerate the paired `.md`
            // for this test so a `DOT_AGENT_DECK_RECORD=1` run keeps
            // the doc next to the freshly-written cast in sync with
            // the test source. Cheap (~3 files to parse today);
            // best-effort — a generator error is surfaced to stderr
            // but does NOT poison the test result, because rule 7
            // already catches drift in CI.
            regenerate_paired_doc(&self.test_name);
        }
    }
}

/// Best-effort: regenerate the paired `.md` for the currently-running
/// test. Looks up the test by its Rust thread-name (which is the fn
/// name in cargo test), maps that to a spec id via the discovered
/// `#[spec]` set, and writes the resulting doc. Any error is logged
/// to stderr without panicking — CI's linkage-check rule 7 is the
/// load-bearing enforcement.
fn regenerate_paired_doc(test_name: &str) {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = xtask_docs::DocsConfig::from_workspace(&workspace_root);
    let generated = match xtask_docs::generate_all(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[tui-harness] regenerate paired .md failed: {e}");
            return;
        }
    };
    let target = generated.into_iter().find(|d| d.fn_name == test_name);
    match target {
        Some(g) => {
            if let Err(e) = xtask_docs::write_all(std::slice::from_ref(&g)) {
                eprintln!(
                    "[tui-harness] regenerate paired .md write failed for `{test_name}`: {e}"
                );
            }
        }
        None => {
            eprintln!(
                "[tui-harness] no #[spec(...)] test matches fn name `{test_name}` — skipping doc regeneration"
            );
        }
    }
}

impl TuiDeck {
    fn dump_recordings(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;

        // M4.3: atomic writes for every artifact in the per-test
        // dir. Two `cargo test-e2e` runs on the same checkout (or one
        // run racing `cargo xtask docs --tests` against the `.md`)
        // can land here concurrently for the same test; tempfile +
        // rename inside the destination directory keeps the
        // post-rename file either fully old or fully new — never
        // half-written.

        // final-grid.txt
        let grid = self.snapshot_grid();
        atomic_write(&dir.join("final-grid.txt"), grid.as_bytes())?;

        // final-grid.svg — minimal monospace render. Not pixel-perfect,
        // but valid SVG that opens in any browser.
        let svg = render_grid_to_svg(&grid, self.cols, self.rows);
        atomic_write(&dir.join("final-grid.svg"), svg.as_bytes())?;

        // full-stream.cast — asciinema v2 format (header + one JSON
        // array per event). Inline encoder, ~20 lines.
        let cast = self.encode_asciinema_cast();
        atomic_write(&dir.join("full-stream.cast"), cast.as_bytes())?;

        // fixture.toml — copy of the deck's .dot-agent-deck.toml so a
        // reviewer can replay against the same config.
        let fixture_src = self.fixture_path.join(".dot-agent-deck.toml");
        if fixture_src.exists() {
            let bytes = std::fs::read(&fixture_src)?;
            atomic_write(&dir.join("fixture.toml"), &bytes)?;
        }
        Ok(())
    }

    fn encode_asciinema_cast(&self) -> String {
        let mut s = String::new();
        // Header — minimum required fields for asciinema v2.
        let header = serde_json::json!({
            "version": 2,
            "width": self.cols,
            "height": self.rows,
            "env": {
                "TERM": "xterm-256color",
            },
        });
        s.push_str(&header.to_string());
        s.push('\n');
        let events = self.cast_events.lock().unwrap();
        for ev in events.iter() {
            // Lossy UTF-8 decoding is what asciinema players expect:
            // raw bytes that are valid UTF-8 round-trip, invalid bytes
            // are replaced rather than dropped.
            let data = String::from_utf8_lossy(&ev.data);
            let line = serde_json::json!([ev.offset_secs, "o", data]);
            s.push_str(&line.to_string());
            s.push('\n');
        }
        s
    }
}

// ---------------------------------------------------------------------------
// L1 buffer-render helpers
// ---------------------------------------------------------------------------
//
// Shared by the in-process `TestBackend` render tests (`tests/render_*.rs`),
// which assert on a `ratatui::buffer::Buffer` rather than a PTY grid. Kept here
// so the button-bar and layout suites read a single copy (PRD #144 DRY).

/// Count the rows of `buffer` that carry any non-blank cell — i.e. how many
/// rows the rendered bar actually occupies. One row means the bar fit on a
/// single line; two or more means it wrapped (each extra row is one row the
/// dashboard must cede from its height budget for the bottom bar).
pub fn nonblank_rows(buffer: &ratatui::buffer::Buffer) -> usize {
    let area = buffer.area();
    (0..area.height)
        .filter(|&y| (0..area.width).any(|x| !buffer[(x, y)].symbol().trim().is_empty()))
        .count()
}

/// Join every row of a (possibly multi-row) bar buffer into one `\n`-separated
/// string, for a readable failure message. A wrapped button bar spreads its
/// full-label buttons across more than one row; each label stays contiguous
/// within a single row, so a `\n`-joined `.contains(label)` finds it without
/// crossing the boundary.
pub fn joined_rows(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk `needles` against `haystack` in order, returning how many
/// elements of `needles` matched. The N-th element must be found at
/// an offset strictly greater than the offset that matched the
/// (N-1)-th element. Used by [`TuiDeck::wait_for_strings_in_order`]
/// and exercised by the unit tests below — extracted so the polling
/// logic stays trivial and the matching invariant is testable
/// without spawning a PTY.
fn match_needles_in_order(haystack: &[u8], needles: &[&str]) -> usize {
    let text = String::from_utf8_lossy(haystack);
    let mut cursor = 0usize;
    let mut matched = 0usize;
    for needle in needles {
        match text[cursor..].find(needle) {
            Some(rel_idx) => {
                let abs_end = cursor + rel_idx + needle.len();
                cursor = abs_end;
                matched += 1;
            }
            None => break,
        }
    }
    matched
}

fn locate_fixture(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is the repo root for integration tests.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = root.join("tests").join("fixtures").join(name);
    assert!(p.is_dir(), "fixture directory missing: {p:?}");
    p
}

fn copy_dir_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursively(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            // M2.1 auditor Nit 3: refuse to copy symlinks / sockets /
            // FIFOs from a fixture. Fixtures are plain files only —
            // a symlink at copy time most likely indicates a fixture
            // bug (or an attacker pre-staging a symlink targeting the
            // tempdir's parent), so surface it loudly instead of
            // silently skipping.
            return Err(std::io::Error::other(format!(
                "fixture entry {} is not a regular file or directory \
                 (symlinks/sockets/FIFOs are not supported in fixtures)",
                from.display()
            )));
        }
    }
    Ok(())
}

/// Workspace-relative `.dot-agent-deck/recordings/` resolved to an
/// ABSOLUTE path at harness construction time. The fixture-copy step
/// `cwd`s the deck into a per-test tempdir, so any relative path here
/// would land artifacts in the wrong place. M4.3: artifacts moved
/// from `target/test-recordings/<run-id>/<test>/` to
/// `.dot-agent-deck/recordings/<test>/` — gitignored dev-time state,
/// no per-run subdir (concurrent runs on one checkout aren't a
/// real-world workflow).
fn workspace_recordings_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".dot-agent-deck")
        .join("recordings")
}

/// Atomic file write: stage `bytes` in a sibling tempfile under
/// `dst.parent()` and then `persist(dst)` so the rename is atomic on
/// Unix (same filesystem). Concurrent writers see either the
/// previous or the new file, never a half-written one.
fn atomic_write(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = dst.parent().ok_or_else(|| {
        std::io::Error::other(format!("dump path has no parent: {}", dst.display()))
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tui-harness-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    std::io::Write::write_all(tmp.as_file_mut(), bytes)?;
    tmp.as_file().sync_all().ok();
    tmp.persist(dst).map_err(|e| e.error)?;
    Ok(())
}

fn current_test_name() -> String {
    // Rust unit tests run on threads named after the test function.
    // Falls back to a placeholder when called off-thread.
    std::thread::current()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unnamed-test".to_string())
}

fn sanitize_test_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render a parsed grid as a minimal monospace SVG. Each row becomes
/// one `<text>` element; cells get no per-attribute styling — colors
/// would need attribute tracking which is more than the failure-dump
/// surface needs. Reviewers replay the cast for color.
fn render_grid_to_svg(grid: &str, cols: u16, rows: u16) -> String {
    let cell_w = 8;
    let cell_h = 16;
    let width = cols as usize * cell_w;
    let height = rows as usize * cell_h;
    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n"
    ));
    s.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#0c0c0c\"/>\n");
    s.push_str("<style>text { font-family: monospace; font-size: 13px; fill: #d0d0d0; }</style>\n");
    for (i, line) in grid.lines().enumerate() {
        let y = (i + 1) * cell_h;
        let escaped = line
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        s.push_str(&format!(
            "<text x=\"0\" y=\"{y}\" xml:space=\"preserve\">{escaped}</text>\n"
        ));
    }
    s.push_str("</svg>\n");
    s
}

/// PRD #77 Decision 26 runtime-skip helper: returns `Ok(())` when the
/// host has the Claude Code CLI on PATH and a readable credentials
/// file; `Err(reason)` with a stable user-facing message otherwise.
/// Tests pair this with [`skip_unless!`].
pub fn check_claude_available() -> Result<(), String> {
    if !cli_invocable("claude") {
        return Err("Claude Code CLI not installed (could not invoke `claude --version`)".into());
    }
    let home = host_home();
    let creds = home.join(".claude").join(".credentials.json");
    if !creds.exists() {
        // M3.1 auditor S1: surface the abstract path so the message
        // doesn't leak whether the operator is on `/Users/<name>` vs
        // `/root` vs `/home/<name>`.
        return Err(
            "Claude Code credentials not found at ~/.claude/.credentials.json — \
             log in with `claude login`"
                .into(),
        );
    }
    Ok(())
}

/// PRD #77 Decision 26 runtime-skip helper for OpenCode. Mirrors
/// [`check_claude_available`] — checks for the CLI on PATH and an
/// OpenCode auth.json (or analogous credential the user logged in
/// with).
pub fn check_opencode_available() -> Result<(), String> {
    if !cli_invocable("opencode") {
        return Err("OpenCode CLI not installed (could not invoke `opencode --version`)".into());
    }
    let home = host_home();
    let candidates = [
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json"),
        home.join(".opencode").join("auth.json"),
        home.join(".config").join("opencode").join("auth.json"),
    ];
    if candidates.iter().any(|p| p.exists()) {
        return Ok(());
    }
    // M3.1 auditor S1: redact $HOME in the surfaced path.
    Err(
        "OpenCode credentials not found at ~/.local/share/opencode/auth.json — \
         log in with `opencode auth login`"
            .into(),
    )
}

/// Helper: returns true when `bin --version` exits 0, false otherwise
/// (binary missing, returns non-zero, etc.). Used by the
/// `check_*_available()` helpers — extracted so the BoolNot trait
/// from M2 can be retired (M3.1 auditor Nit 5).
fn cli_invocable(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Body of the `skip_unless!` early-return: if `result` is `Err`,
/// print `SKIP: <reason>` to stderr and indicate to the caller it
/// should return. Pairs with the `skip_unless!` macro below.
#[doc(hidden)]
pub fn _skip_if_err(result: Result<(), String>) -> bool {
    match result {
        Ok(()) => false,
        Err(reason) => {
            eprintln!("SKIP: {reason}");
            true
        }
    }
}

/// Decision 26 / Decision 8 runtime-skip shorthand. Use at the top
/// of a chain-smoke test:
///
/// ```ignore
/// skip_unless!(common::check_claude_available());
/// ```
///
/// Prints `SKIP: <reason>` to stderr and returns from the calling
/// function when the environment isn't capable of running the test.
#[macro_export]
macro_rules! skip_unless {
    ($expr:expr) => {
        if $crate::common::_skip_if_err($expr) {
            return;
        }
    };
}

/// Host user's HOME directory at test-runner launch time, used by
/// the credential-availability checks and the credential-import copy
/// path. Resolved from the parent process's env (not from the
/// already-redirected per-test tempdir HOME).
fn host_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set on the host"))
}

/// Copy the host user's Claude Code credentials + settings into the
/// per-test tempdir HOME. Strips any `hooks` entries from the
/// imported `settings.json` (the deck auto-installs its own hooks
/// pointing at the per-test socket — leaving the host's hook entries
/// in place would invoke the developer's real hook commands inside
/// the test). M3.1 auditor S2 + S3: write the destination with mode
/// 0o600 atomically; refuse source files that are symlinks.
fn import_claude_credentials(test_home: &Path) -> std::io::Result<()> {
    let src_root = host_home().join(".claude");
    let dst_root = test_home.join(".claude");
    std::fs::create_dir_all(&dst_root)?;

    let src_creds = src_root.join(".credentials.json");
    let creds_bytes = read_credential_file_no_symlink(
        &src_creds,
        "Claude Code credentials not found at ~/.claude/.credentials.json — \
         log in with `claude login`",
        "~/.claude/.credentials.json",
    )?;
    write_credential_file_atomic_0o600(&dst_root.join(".credentials.json"), &creds_bytes)?;

    // settings.json: copy if present, with `hooks` stripped. Claude's
    // settings.json is JSONC (line + block comments) — M3.1 auditor
    // S0 fix: strip comments before serde_json parse so the strip is
    // never a no-op on a real settings.json with `// foo` lines.
    // M4.6 P2: settings.json can carry the same tokens / sensitive
    // config that motivate the 0o600 mode on credentials.json, so
    // route it through the same atomic-0o600 helper rather than
    // inheriting umask via fs::write. `write_credential_file_atomic_0o600`
    // treats its input as opaque bytes — the JSONC body comes out
    // intact.
    let src_settings = src_root.join("settings.json");
    if src_settings.exists() {
        require_regular_file_no_symlink(&src_settings, "~/.claude/settings.json")?;
        let raw = std::fs::read_to_string(&src_settings)?;
        let dst_text = strip_hooks_from_claude_settings(&raw)?;
        write_credential_file_atomic_0o600(&dst_root.join("settings.json"), dst_text.as_bytes())?;
    }

    // plugins/ (and any other supporting dirs) — best-effort copy if
    // present. `copy_dir_recursively` was further tightened in M3
    // from M2.1 Nit 3's "silent skip" to a hard refuse on any
    // non-regular entry (symlinks/sockets/FIFOs), so this branch
    // already shares the credential-side stance on symlinks.
    let src_plugins = src_root.join("plugins");
    if src_plugins.is_dir() {
        require_regular_dir_no_symlink(&src_plugins, "~/.claude/plugins")?;
        copy_dir_recursively(&src_plugins, &dst_root.join("plugins"))?;
    }
    Ok(())
}

/// Strip the top-level `hooks` key from a Claude Code settings.json.
/// settings.json is JSONC: line (`// foo`) and block (`/* foo */`)
/// comments are tolerated by Claude's own loader. M3.1 auditor S0
/// fixes the fail-open path: comments are stripped before parsing so
/// real-world settings.json files (which carry `//` comments) are
/// rewritten with their hook block removed rather than passed
/// through unchanged. A truly-malformed settings.json (still invalid
/// after comment stripping) is now fail-CLOSED — we refuse to
/// continue rather than risk shipping the host's hook commands into
/// the test.
fn strip_hooks_from_claude_settings(raw: &str) -> std::io::Result<String> {
    let cleaned = strip_jsonc_comments(raw);
    let mut v: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        std::io::Error::other(format!(
            "refusing to import host settings.json: not valid JSON(C) after \
             comment-stripping ({e}). Leaving the host's hook entries in place \
             would let them fire inside the test."
        ))
    })?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("hooks");
    }
    serde_json::to_string_pretty(&v)
        .map_err(|e| std::io::Error::other(format!("serialize sanitized settings.json: {e}")))
}

/// Strip `//` line comments and `/* … */` block comments from a
/// JSONC string. Preserves string literals (so `"//"` and `"/*"`
/// inside a quoted value are left alone) and keeps newlines so any
/// downstream parse-error line numbers still align.
fn strip_jsonc_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    let mut block_depth: usize = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let next = bytes.get(i + 1).map(|b| *b as char);

        if block_depth > 0 {
            if c == '*' && next == Some('/') {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '/' && next == Some('/') {
            // Line comment: eat until newline.
            while i < bytes.len() && bytes[i] as char != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Read a credential file, refusing symlinks at the source path
/// (M3.1 auditor S3). Returns the file bytes on success, or a
/// redacted `io::Error` on failure with the abstract `~/` path so
/// the stderr output doesn't leak the host's real $HOME.
fn read_credential_file_no_symlink(
    real_path: &Path,
    not_found_message: &str,
    redacted_display: &str,
) -> std::io::Result<Vec<u8>> {
    let meta = match std::fs::symlink_metadata(real_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::other(not_found_message.to_string()));
        }
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "failed to stat {redacted_display}: {e}"
            )));
        }
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found a symlink"
        )));
    }
    if !file_type.is_file() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found {:?}",
            file_type
        )));
    }
    std::fs::read(real_path)
        .map_err(|e| std::io::Error::other(format!("read {redacted_display}: {e}")))
}

/// Validate that a source path is a regular file (not a symlink),
/// without reading it. Used by paths where we want to surface
/// symlink-rejection before delegating the actual copy/read to a
/// caller.
fn require_regular_file_no_symlink(
    real_path: &Path,
    redacted_display: &str,
) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(real_path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found a symlink"
        )));
    }
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file"
        )));
    }
    Ok(())
}

/// Validate that a source path is a regular directory (not a
/// symlink). Mirrors [`require_regular_file_no_symlink`] for the
/// `~/.claude/plugins` directory copy.
fn require_regular_dir_no_symlink(real_path: &Path, redacted_display: &str) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(real_path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular directory, found a symlink"
        )));
    }
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular directory"
        )));
    }
    Ok(())
}

/// Write `bytes` to `dst` atomically with mode 0o600 — the
/// destination is `open`ed with `O_CREAT | O_WRONLY | O_TRUNC` AND
/// the mode flag set to 0o600 in the same syscall (M3.1 auditor S2),
/// so there is no umask-derived 0o666 window between create and
/// chmod. Refuses to follow if `dst` already exists as a symlink.
fn write_credential_file_atomic_0o600(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Pre-remove any existing entry — `OpenOptions::create + mode` on
    // an existing file does not re-stamp the mode, and we want a
    // freshly-zeroed credential file with the strict mode regardless
    // of what was there before.
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::other(format!(
                "refusing to write credential into existing symlink at {}",
                dst.display()
            )));
        }
        Ok(_) => {
            std::fs::remove_file(dst).ok();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)?;
        f.write_all(bytes)?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(dst, bytes)?;
    }
    Ok(())
}

/// Copy the host user's OpenCode credentials into the per-test
/// tempdir HOME. Mirrors [`import_claude_credentials`] — copies the
/// auth state but NOT any `plugin/` directory (the deck installs its
/// own OpenCode plugin pointing at the per-test paths). M3.1
/// auditor S2 + S3: atomic 0o600 creation for `auth.json`, and
/// source-path symlinks are refused with a redacted error.
///
/// This helper is currently dead code (no `chain-smoke/opencode/*`
/// test calls it — see PRD § Discovered Issues `di-001`). Kept so
/// the OpenCode chain-smoke test can be added without harness
/// changes once the deck install-path bug is fixed.
fn import_opencode_credentials(test_home: &Path) -> std::io::Result<()> {
    let mut imported_any = false;

    let source_roots = [
        host_home().join(".local").join("share").join("opencode"),
        host_home().join(".opencode"),
    ];
    let redacted_roots = ["~/.local/share/opencode", "~/.opencode"];
    for (src, redacted) in source_roots.iter().zip(redacted_roots.iter()) {
        // Stat with symlink_metadata so a symlinked root is refused
        // rather than silently followed.
        let Ok(meta) = std::fs::symlink_metadata(src) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "refusing to import {redacted}: expected a regular directory, found a symlink"
            )));
        }
        if !meta.file_type().is_dir() {
            continue;
        }
        let rel = src
            .strip_prefix(host_home())
            .expect("HOME-relative source path");
        let dst = test_home.join(rel);
        copy_dir_excluding_plugin_subdir(src, &dst)?;
        // Re-stamp auth.json with the strict mode atomically — the
        // dir-copy walks regular files via fs::copy which inherits
        // host mode bits.
        let dst_auth = dst.join("auth.json");
        if dst_auth.is_file() {
            let bytes = std::fs::read(&dst_auth)?;
            write_credential_file_atomic_0o600(&dst_auth, &bytes)?;
        }
        imported_any = true;
    }

    // ~/.config/opencode/opencode.jsonc is the user-editable config.
    let src_cfg = host_home()
        .join(".config")
        .join("opencode")
        .join("opencode.jsonc");
    if src_cfg.exists() {
        require_regular_file_no_symlink(&src_cfg, "~/.config/opencode/opencode.jsonc")?;
        let dst_cfg_dir = test_home.join(".config").join("opencode");
        std::fs::create_dir_all(&dst_cfg_dir)?;
        std::fs::copy(&src_cfg, dst_cfg_dir.join("opencode.jsonc"))?;
        imported_any = true;
    }

    if !imported_any {
        return Err(std::io::Error::other(
            "OpenCode credentials not found under ~/.local/share/opencode or ~/.opencode — \
             log in with `opencode auth login`"
                .to_string(),
        ));
    }
    Ok(())
}

/// Like `copy_dir_recursively` but skips any top-level `plugin/`
/// child — the deck auto-installs its own OpenCode plugin into the
/// tempdir HOME and we do NOT want the host's plugin firing too.
fn copy_dir_excluding_plugin_subdir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            if entry.file_name() == "plugin" {
                continue;
            }
            copy_dir_recursively(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            return Err(std::io::Error::other(format!(
                "OpenCode credential entry {} is not a regular file or directory \
                 (symlinks/sockets/FIFOs are not supported)",
                from.display()
            )));
        }
    }
    Ok(())
}

/// Write a minimal `session.toml` containing exactly one pane that
/// runs `command` in `work_dir`. The deck reads this when launched
/// with `--continue`.
fn write_continue_session_file(
    session_toml_path: &Path,
    work_dir: &Path,
    pane_name: &str,
    command: &str,
) -> std::io::Result<()> {
    // Hand-rolled TOML so we don't need a runtime dep on toml in the
    // harness module. Field names match `dot_agent_deck::config::SavedPane`.
    let mut s = String::new();
    s.push_str("[[panes]]\n");
    s.push_str(&format!(
        "dir = \"{}\"\n",
        toml_escape(work_dir.to_str().expect("work dir is UTF-8"))
    ));
    s.push_str(&format!("name = \"{}\"\n", toml_escape(pane_name)));
    s.push_str(&format!("command = \"{}\"\n", toml_escape(command)));
    std::fs::write(session_toml_path, s)
}

/// Escape `s` so it can be embedded as a TOML basic string between
/// `"…"`. M3.1 auditor Nit 3 — the original two-replace shape missed
/// control characters and BS/FF/LF/CR/TAB, any of which would
/// produce an invalid TOML file. We follow the TOML 1.0 spec: `\b`,
/// `\t`, `\n`, `\f`, `\r`, `\\`, `\"` are the literal escapes; other
/// control chars (U+0000..=U+001F minus the named ones, plus U+007F)
/// take the `\uXXXX` form. UTF-8 codepoints above the C0 range are
/// allowed in basic strings as-is.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// Helper for L2 tests: send a single JSON line to the deck's hook
/// socket. Connects, writes the line + newline, and drops the
/// connection. Synthetic-event tests use this to inject events
/// without going through the `hook` subcommand.
pub fn write_hook_line(socket: &Path, json_line: &str) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    // The daemon binds the hook socket asynchronously after the TUI
    // is up; retry briefly if it is not yet present.
    let mut last_err = None;
    while Instant::now() < deadline {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(mut stream) => {
                stream.write_all(json_line.as_bytes())?;
                if !json_line.ends_with('\n') {
                    stream.write_all(b"\n")?;
                }
                stream.flush()?;
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("timed out waiting for hook socket")))
}

// ---------------------------------------------------------------------------
// Legacy test helpers
// ---------------------------------------------------------------------------
//
// Carried forward from the pre-M1 `tests/common/mod.rs`. The M1 audit moved
// the originals into `tmp/legacy-tests/` (Decision 10), but a subset of
// integration tests on `main` — `tests/daemon_protocol.rs`,
// `tests/rehydration.rs`, `tests/spawn_time_role_prompt_submit_after_session_start.rs`,
// `tests/snapshot_replay_dims.rs` — keep calling these helpers via
// `common::*`. Restored here so the merge with `main` builds. Per the
// M5+ "absorbed into per-PRD test maintenance" decision, the legacy
// integration tests are grandfathered until a future PRD refactors
// them onto the PRD #77 harness.
//
// The `dot_agent_deck::daemon::run_daemon_with` lock-root context that
// drove the original helpers (`flock(2)` over a per-socket `.lock`
// resolved via `XDG_RUNTIME_DIR` / `$HOME/.cache`) is documented in
// `tmp/legacy-tests/tests/common/mod.rs`; only the surface those tests
// import is reproduced here.

use std::os::unix::fs::PermissionsExt as _LegacyPermissionsExt;
use std::sync::OnceLock;

#[allow(dead_code)]
static LOCK_DIR_GUARD: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Idempotent setup hook for legacy daemon-spawning tests. Creates the
/// per-binary lock-dir tempdir on first call; subsequent calls are
/// no-ops.
#[allow(dead_code)]
pub fn init_test_env() {
    LOCK_DIR_GUARD.get_or_init(|| {
        tempfile::Builder::new()
            .prefix("dot-agent-deck-test-lock-")
            .tempdir()
            .expect("create per-binary lock-dir tempdir")
    });
}

/// Path to the per-binary lock-dir tempdir, for passing to
/// `dot_agent_deck::daemon::Daemon::with_lock_dir_override` (in-process
/// tests) or to `Command::env` for subprocess-based tests. Returns
/// `None` if [`init_test_env`] was never called.
#[allow(dead_code)]
pub fn lock_dir_path() -> Option<PathBuf> {
    LOCK_DIR_GUARD.get().map(|d| d.path().to_path_buf())
}

/// Race-safe `tempfile::tempdir()` wrapper: re-applies 0o700 after
/// creation so the per-test directory survives the daemon's
/// `bind_socket` umask flip. Mirrors `src/daemon_attach.rs`'s
/// same-named helper; promoted here so every legacy daemon-spawning
/// test binary gets the fix without duplicating the workaround.
#[allow(dead_code)]
pub fn race_safe_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("chmod tempdir to 0o700");
    dir
}

// ---------------------------------------------------------------------------
// PRD #127 Phase 1 — headless `daemon serve` harness
// ---------------------------------------------------------------------------
//
// The scheduler lives in the daemon, not the TUI, so its L2 tests drive the
// real `dot-agent-deck daemon serve` process directly (no PTY / vt100 grid —
// there is no TUI surface to render) and observe it through three channels:
//   - OS process liveness (`try_wait`) for the idle-shutdown carve-out;
//   - the attach socket's `AttachRequest`/`AttachResponse` control protocol
//     for `ReloadSchedules`;
//   - the `dot-agent-deck schedule …` CLI subprocess for the writer + reload
//     trigger.
//
// All sleeping / polling lives in these helpers (in `common`, NOT in an
// `e2e_*.rs` body) so linkage-check Decision 21 (no raw sleeps / fixed-count
// polling in e2e test bodies) is satisfied by construction.

/// A spawned headless `dot-agent-deck daemon serve` process plus the per-test
/// tempdir paths it was pointed at. Drop kills the child so a hung daemon
/// never leaks past the test.
#[allow(dead_code)]
pub struct DaemonProc {
    child: std::process::Child,
    /// Hook-ingestion socket (`DOT_AGENT_DECK_SOCKET`).
    pub hook_socket: PathBuf,
    /// Streaming attach / control socket (`DOT_AGENT_DECK_ATTACH_SOCKET`).
    pub attach_socket: PathBuf,
    /// Global schedules config (`DOT_AGENT_DECK_SCHEDULES`); the writer's
    /// fixed target regardless of cwd.
    pub schedules_path: PathBuf,
    /// Per-test HOME.
    pub home: PathBuf,
    /// Env the daemon was launched with, replayed onto every `schedule` CLI
    /// subprocess so the CLI and daemon share sockets + the schedules path.
    env: Vec<(String, String)>,
    /// Captured daemon stderr (the `StderrNotifier` failure-surfacing seam
    /// writes here via `eprintln!`).
    stderr_path: PathBuf,
    _tempdir: tempfile::TempDir,
}

#[allow(dead_code)]
impl Drop for DaemonProc {
    fn drop(&mut self) {
        // The daemon was spawned in its own process group (pgid == its pid),
        // so a negative-pid SIGKILL reaps the whole tree — the daemon and any
        // agents it spawned — in one shot. Best-effort; ignore ESRCH/EPERM.
        let pid = self.child.id();
        // SAFETY: kill(2) with a negative pid signals the process group;
        // SIGKILL has no failure mode beyond ESRCH/EPERM, which we ignore.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `dot-agent-deck daemon serve` headlessly against an isolated tempdir.
///
/// `initial_schedules_toml` seeds the global `schedules.toml` (None = no file,
/// i.e. an empty schedule set). `idle_shutdown_secs` is passed verbatim as
/// `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` ("0" disables idle shutdown; a small
/// number arms a fast idle window for the carve-out test). Blocks until the
/// attach socket appears so callers can immediately drive the control protocol.
#[allow(dead_code)]
pub fn spawn_daemon_serve(
    initial_schedules_toml: Option<&str>,
    idle_shutdown_secs: &str,
) -> DaemonProc {
    spawn_daemon_serve_with_env(initial_schedules_toml, idle_shutdown_secs, &[])
}

/// Like [`spawn_daemon_serve`] but layers `extra_env` onto the daemon's
/// environment (and onto every `schedule` CLI subprocess). Used by the spawn
/// tests to pin `SHELL` for the `$SHELL`-fallback case.
#[allow(dead_code)]
pub fn spawn_daemon_serve_with_env(
    initial_schedules_toml: Option<&str>,
    idle_shutdown_secs: &str,
    extra_env: &[(&str, &str)],
) -> DaemonProc {
    let tempdir = race_safe_tempdir();
    let work = tempdir.path().to_path_buf();
    let home = work.join("home");
    std::fs::create_dir_all(&home).expect("create per-test HOME");
    let state_dir = work.join("state");
    let hook_socket = work.join("hook.sock");
    let attach_socket = work.join("attach.sock");
    let schedules_path = work.join("schedules.toml");
    if let Some(toml) = initial_schedules_toml {
        std::fs::write(&schedules_path, toml).expect("seed schedules.toml");
    }

    let mut env: Vec<(String, String)> = Vec::new();
    if let Ok(p) = std::env::var("PATH") {
        env.push(("PATH".into(), p));
    }
    env.push(("HOME".into(), home.to_string_lossy().into_owned()));
    env.push(("TERM".into(), "xterm-256color".into()));
    env.push((
        "DOT_AGENT_DECK_SOCKET".into(),
        hook_socket.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
        attach_socket.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_SCHEDULES".into(),
        schedules_path.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(),
        idle_shutdown_secs.to_string(),
    ));
    // Leaked-daemon safety net. DaemonProc spawns `daemon serve` as a
    // NON-detached child (its parent is this test process), so the orphan
    // watchdog fires correctly when the test dies without running `Drop`
    // (SIGKILL / panic-abort / nextest timeout / Ctrl-C) — the daemon
    // gracefully self-exits instead of leaking to PID 1. The max-lifetime cap
    // is a belt-and-suspenders backstop for anything the watchdog misses.
    // `IDLE_SHUTDOWN_SECS` is left as the caller passed it (tests rely on `0`
    // for determinism). These vars are inert for the short-lived `schedule`
    // CLI subprocesses that also replay this env — only `daemon serve` reads
    // them.
    env.push(("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED".into(), "1".into()));
    env.push(("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS".into(), "300".into()));
    // PRD #127: the scheduler spawn primitive gates a fresh fire's prompt
    // delivery on the spawned agent's `SessionStart` (readiness), falling back
    // after a timeout for commands that emit no hook (bare `cat`, the recorder
    // scripts these tests use). Shrink that fallback from the production 10s so
    // the no-hook delivery tests don't race their ~10s observation windows;
    // 5000ms stays comfortably above spawn/005's 2s "not yet delivered" window
    // and below every 10s delivery window. A test may override via `extra_env`.
    env.push(("DOT_AGENT_DECK_SESSION_START_WAIT_MS".into(), "5000".into()));
    for (k, v) in extra_env {
        env.push(((*k).to_string(), (*v).to_string()));
    }

    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("daemon").arg("serve");
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    // Capture stderr to a file so tests can observe the scheduler's
    // failure-surfacing notifications (`StderrNotifier` → `eprintln!`).
    let stderr_path = work.join("daemon-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create daemon stderr log");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::from(stderr_file));
    // Put the daemon in its own process group (pgid == its pid) so `Drop` can
    // reap the WHOLE tree — the daemon plus any agents it spawned — with one
    // `kill(-pgid)`, not just the daemon itself.
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().expect("spawn `dot-agent-deck daemon serve`");

    let proc = DaemonProc {
        child,
        hook_socket,
        attach_socket,
        schedules_path,
        home,
        env,
        stderr_path,
        _tempdir: tempdir,
    };
    proc.wait_for_attach_socket();
    proc
}

#[allow(dead_code)]
impl DaemonProc {
    /// Block until the attach socket file exists (the daemon finished
    /// binding) or a bounded timeout elapses.
    fn wait_for_attach_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.attach_socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "daemon never bound its attach socket at {} within 10s",
            self.attach_socket.display()
        );
    }

    /// Whether the daemon process is still running.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Public point-in-time liveness check (the daemon process has not exited).
    pub fn is_alive_public(&mut self) -> bool {
        self.is_alive()
    }

    /// Assert the daemon stays alive for the whole `window` — polls
    /// throughout so an early exit fails fast with a clear message rather
    /// than passing on a lucky end-of-window sample.
    pub fn assert_alive_for(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if !self.is_alive() {
                panic!(
                    "daemon exited within {window:?} but was expected to stay alive \
                     (idle-shutdown carve-out for a registered enabled schedule)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Poll until the daemon process exits, returning `true` if it exited
    /// within `timeout` and `false` otherwise.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_alive() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.is_alive()
    }

    /// Send one `AttachRequest` over the control socket and read back the
    /// single `AttachResponse`. Blocking; used to drive `ReloadSchedules`.
    pub fn send_attach_request(
        &self,
        req: &dot_agent_deck::daemon_protocol::AttachRequest,
    ) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
        attach_request_on(&self.attach_socket, req)
    }

    /// Run `dot-agent-deck schedule <args…>` with the daemon's env, from the
    /// tempdir's HOME as cwd. Returns the captured process output.
    pub fn run_schedule_cli(&self, args: &[&str]) -> std::process::Output {
        self.run_schedule_cli_from(&self.home.clone(), args)
    }

    /// Run `dot-agent-deck schedule <args…>` from an explicit `cwd` (used to
    /// prove the writer targets the global path regardless of cwd).
    pub fn run_schedule_cli_from(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("schedule");
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.output()
            .expect("run `dot-agent-deck schedule` subprocess")
    }

    /// Probe the daemon's in-memory registry by issuing `schedule run-now
    /// --name <name>` until it exits 0 (task registered) or a bounded timeout
    /// elapses. `run-now` hits the daemon over the socket and errors on an
    /// unknown task, so a clean exit proves the task is live in the registry.
    pub fn wait_for_schedule_registered(&self, name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let out = self.run_schedule_cli(&["run-now", "--name", name]);
            if out.status.success() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Fire a registered task immediately via the `RunNow` control message
    /// (no file write). Returns the daemon's response.
    pub fn run_now(
        &self,
        name: &str,
    ) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
        self.send_attach_request(&dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        })
    }

    /// Snapshot the daemon's live agent registry via `ListAgents`.
    pub fn agent_records(&self) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
        let resp = self
            .send_attach_request(&dot_agent_deck::daemon_protocol::AttachRequest::ListAgents)
            .expect("ListAgents over the attach socket");
        resp.agent_records.unwrap_or_default()
    }

    /// Poll `ListAgents` until at least `n` agents are registered (or a bounded
    /// timeout elapses), then return the current snapshot. The returned vec may
    /// be shorter than `n` if the timeout fired — callers assert on `.len()`.
    pub fn wait_for_agent_count(
        &self,
        n: usize,
        timeout: Duration,
    ) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
        let deadline = Instant::now() + timeout;
        loop {
            let records = self.agent_records();
            if records.len() >= n || Instant::now() >= deadline {
                return records;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Poll `ListAgents` until a registered agent matches `pred` (or a bounded
    /// timeout elapses); returns the first match. Lets a test wait for a
    /// specific KIND of agent (e.g. a non-orchestration single-agent card)
    /// without an inline poll loop in the e2e body (Decision 21).
    pub fn wait_for_agent_where(
        &self,
        pred: impl Fn(&dot_agent_deck::agent_pty::AgentRecord) -> bool,
        timeout: Duration,
    ) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = self.agent_records().into_iter().find(&pred) {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Assert the registered agent count does NOT exceed `max` for the whole
    /// `window` — used to catch a double-spawn (a fire that opens two tabs).
    pub fn assert_agent_count_stays_at_most(&self, max: usize, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let n = self.agent_records().len();
            assert!(
                n <= max,
                "agent count grew to {n}, expected at most {max} (double-spawn?)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Attach to an agent's PTY stream and read STREAM_OUT bytes until `needle`
    /// appears in the cumulative output (proving the daemon delivered/echoed
    /// the prompt) or a bounded timeout elapses. Returns whether it was seen.
    pub fn attach_and_wait_for_output(
        &self,
        agent_id: &str,
        needle: &str,
        timeout: Duration,
    ) -> bool {
        self.attach_and_wait_for_occurrences(agent_id, needle, 1, timeout)
    }

    /// Like [`attach_and_wait_for_output`] but waits until `needle` has appeared
    /// at least `want` (non-overlapping) times in the cumulative STREAM_OUT —
    /// used to prove a SECOND delivery landed in a REUSED pane (the prompt text
    /// is fixed per task, so a reuse fire shows the same marker twice).
    ///
    /// A fresh attach replays the daemon's scrollback first, so the count
    /// reflects every delivery the pane has seen, not just live bytes.
    pub fn attach_and_wait_for_occurrences(
        &self,
        agent_id: &str,
        needle: &str,
        want: usize,
        timeout: Duration,
    ) -> bool {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, KIND_STREAM_OUT};
        use std::io::{Read, Write};

        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.attach_socket) else {
            return false;
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

        let req = dot_agent_deck::daemon_protocol::AttachRequest::AttachStream {
            id: agent_id.to_string(),
        };
        let payload = serde_json::to_vec(&req).expect("serialize AttachStream");
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        if stream.write_all(&header).is_err() || stream.write_all(&payload).is_err() {
            return false;
        }
        let _ = stream.flush();

        let mut acc: Vec<u8> = Vec::new();
        let needle_bytes = needle.as_bytes();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut fh = [0u8; 5];
            match stream.read_exact(&mut fh) {
                Ok(()) => {}
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return false,
            }
            let kind = fh[0];
            let len = u32::from_be_bytes([fh[1], fh[2], fh[3], fh[4]]) as usize;
            let mut body = vec![0u8; len];
            if len > 0 && read_exact_with_deadline(&mut stream, &mut body, deadline).is_err() {
                return false;
            }
            if kind == KIND_STREAM_OUT {
                acc.extend_from_slice(&body);
                if count_occurrences(&acc, needle_bytes) >= want {
                    return true;
                }
            } else if kind == KIND_RESP {
                continue;
            }
        }
        false
    }

    /// Simulate a user keystroke into a pane: attach to `agent_id` and send one
    /// STREAM_IN frame carrying `input`. The daemon forwards it to the PTY
    /// stdin; for the deliver-on-idle contract the daemon also records this as
    /// the pane's most-recent USER input (the debounce clock). Confirms the
    /// input reached the PTY by waiting for its echo before returning, which
    /// also guarantees the daemon has processed (and timestamped) it.
    pub fn send_pane_input(&self, agent_id: &str, input: &str) -> bool {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_STREAM_IN};
        use std::io::Write;

        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.attach_socket) else {
            return false;
        };
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();

        let req = dot_agent_deck::daemon_protocol::AttachRequest::AttachStream {
            id: agent_id.to_string(),
        };
        let payload = serde_json::to_vec(&req).expect("serialize AttachStream");
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        if stream.write_all(&header).is_err() || stream.write_all(&payload).is_err() {
            return false;
        }
        // STREAM_IN frame with the keystroke bytes.
        let inb = input.as_bytes();
        let mut ih = [0u8; 5];
        ih[0] = KIND_STREAM_IN;
        ih[1..5].copy_from_slice(&(inb.len() as u32).to_be_bytes());
        if stream.write_all(&ih).is_err() || stream.write_all(inb).is_err() {
            return false;
        }
        let _ = stream.flush();
        // Hold the connection open briefly so the daemon drains the STREAM_IN
        // before the socket closes (defensive; the kernel buffers regardless).
        std::thread::sleep(Duration::from_millis(50));
        drop(stream);
        // Confirm the keystroke reached the PTY (and was timestamped) by
        // observing its echo on a fresh attach.
        self.attach_and_wait_for_output(agent_id, input, Duration::from_secs(5))
    }

    /// Whether the captured daemon stderr currently contains `needle`.
    pub fn stderr_contains(&self, needle: &str) -> bool {
        std::fs::read_to_string(&self.stderr_path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
    }

    /// Poll the captured daemon stderr until it contains `needle` or a bounded
    /// timeout elapses.
    pub fn wait_for_stderr_contains(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.stderr_contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

/// Count non-overlapping occurrences of `needle` in `hay`.
#[allow(dead_code)]
fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Send one `AttachRequest` over a daemon attach socket and read back the
/// single `AttachResponse`. Blocking; shared by `DaemonProc` and the
/// `TuiDeck`-driven tests (which pass `deck.attach_socket_path()`).
#[allow(dead_code)]
pub fn attach_request_on(
    socket: &Path,
    req: &dot_agent_deck::daemon_protocol::AttachRequest,
) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
    use std::io::{Read, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let payload = serde_json::to_vec(req).expect("serialize AttachRequest");
    let mut header = [0u8; 5];
    header[0] = KIND_REQ;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut resp_header = [0u8; 5];
    stream.read_exact(&mut resp_header)?;
    if resp_header[0] != KIND_RESP {
        return Err(std::io::Error::other(format!(
            "expected RESP frame, got kind 0x{:02x}",
            resp_header[0]
        )));
    }
    let len = u32::from_be_bytes([
        resp_header[1],
        resp_header[2],
        resp_header[3],
        resp_header[4],
    ]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

/// Snapshot a daemon's live agent registry via `ListAgents` over `socket`.
#[allow(dead_code)]
pub fn agent_records_on(socket: &Path) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
    match attach_request_on(
        socket,
        &dot_agent_deck::daemon_protocol::AttachRequest::ListAgents,
    ) {
        Ok(resp) => resp.agent_records.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Poll `ListAgents` until an agent whose `display_name` equals `name` is
/// present (`want_present = true`) or absent (`want_present = false`), or the
/// timeout elapses. Returns whether the desired condition held.
#[allow(dead_code)]
pub fn wait_for_agent_display_name(
    socket: &Path,
    name: &str,
    want_present: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let present = agent_records_on(socket)
            .iter()
            .any(|r| r.display_name.as_deref() == Some(name));
        if present == want_present {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Count occurrences of `needle` in a file's contents (lossy UTF-8). Returns 0
/// if the file is missing/unreadable. Used to count prompt deliveries recorded
/// by a per-pane "recorder" command (one appended line per delivered prompt),
/// which is immune to PTY echo doubling.
#[allow(dead_code)]
pub fn count_file_substr(path: &Path, needle: &str) -> usize {
    match std::fs::read(path) {
        Ok(bytes) => count_occurrences(&bytes, needle.as_bytes()),
        Err(_) => 0,
    }
}

/// Poll until `needle` appears at least `want` times in `path` (or a bounded
/// timeout elapses). Returns whether the count was reached.
#[allow(dead_code)]
pub fn wait_for_file_substr_count(
    path: &Path,
    needle: &str,
    want: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if count_file_substr(path, needle) >= want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll until `name` no longer appears in the file at `path` (e.g. a schedule
/// definition removed from `schedules.toml`), or the timeout elapses.
#[allow(dead_code)]
pub fn wait_for_schedule_absent_from_file(path: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let absent = match std::fs::read_to_string(path) {
            Ok(s) => !s.contains(name),
            Err(_) => true, // file gone entirely → definitely absent
        };
        if absent {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bounded poll for a filesystem path to appear. Kept in `common` so e2e test
/// bodies don't carry a raw sleep loop (linkage-check Decision 21).
#[allow(dead_code)]
pub fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists()
}

/// Blocking `read_exact` bounded by a wall-clock `deadline`, tolerating the
/// per-read timeout set on the stream. Returns `Err` on EOF / hard error / the
/// deadline passing before the buffer fills.
#[allow(dead_code)]
fn read_exact_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline elapsed mid-frame",
            ));
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof",
                ));
            }
            Ok(n) => filled += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Poll `cond` until it returns `true` or `timeout` elapses; returns the final
/// value. Decision 21: bounded polling lives in `common`, never in an
/// `e2e_*.rs` body (which forbids raw `sleep`).
#[allow(dead_code)]
pub fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Whether `pid` is still a live (non-exited) process. A reaped pid is gone; a
/// reparented-then-exited pid may briefly be a zombie — treat state `Z` as
/// exited so the check isn't fooled by an unreaped zombie under a sub-reaper.
/// Uses `/proc` on Linux and falls back to a `kill(pid, 0)` probe elsewhere.
#[allow(dead_code)]
pub fn process_running(pid: i32) -> bool {
    let stat_path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(&stat_path) {
        Ok(stat) => match stat.rfind(')') {
            // `/proc/<pid>/stat` is `pid (comm) STATE ...`; comm may contain
            // spaces/parens, so the state char follows the last ')'.
            Some(idx) => stat[idx + 1..].trim_start().chars().next() != Some('Z'),
            None => true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if Path::new("/proc").is_dir() {
                false // Linux: no /proc entry → the pid is gone.
            } else {
                // SAFETY: kill(pid, 0) only probes existence/permission.
                unsafe { libc::kill(pid, 0) == 0 }
            }
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod harness_unit_tests {
    use super::*;

    #[test]
    fn strip_jsonc_comments_drops_line_and_block_comments() {
        let input = "{\n  // line comment\n  /* block\n  comment */ \"a\": 1\n}";
        let out = strip_jsonc_comments(input);
        // serde_json must be able to parse the result without the
        // JSONC comment tokens.
        let v: serde_json::Value = serde_json::from_str(&out).expect("stripped output parses");
        assert_eq!(v["a"], serde_json::json!(1));
    }

    #[test]
    fn strip_jsonc_comments_preserves_string_literal_slashes() {
        let input = r#"{"url": "https://example.com/path", "marker": "//keep" }"#;
        let out = strip_jsonc_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("parses");
        assert_eq!(v["url"], "https://example.com/path");
        assert_eq!(v["marker"], "//keep");
    }

    #[test]
    fn strip_hooks_from_claude_settings_jsonc_input_strips_hooks() {
        // M3.1 auditor S0 regression: a `//`-comment-bearing
        // settings.json must round-trip through the stripper with
        // its `hooks` key removed, NOT pass through unchanged.
        let raw = "{\n  // top-level comment\n  \"hooks\": {\"PostToolUse\": []},\n  \"theme\": \"dark\"\n}";
        let out = strip_hooks_from_claude_settings(raw).expect("jsonc parses after stripping");
        assert!(
            !out.contains("hooks"),
            "stripped settings must not still mention `hooks`: {out}"
        );
        assert!(
            out.contains("\"theme\""),
            "stripped settings must keep non-hook keys: {out}"
        );
    }

    #[test]
    fn strip_hooks_from_claude_settings_truly_malformed_fails_closed() {
        // Garbage that isn't valid JSON even after comment stripping
        // is rejected with an Err — fail-CLOSED rather than letting
        // the host's hooks survive into the test (M3.1 auditor S0).
        let result = strip_hooks_from_claude_settings("{ this is not valid json at all");
        assert!(result.is_err());
        let err_text = result.unwrap_err().to_string();
        assert!(
            err_text.contains("not valid JSON"),
            "error must explain why the file was rejected: {err_text}"
        );
    }

    #[test]
    fn toml_escape_passes_plain_strings_through() {
        assert_eq!(toml_escape("simple"), "simple");
        assert_eq!(toml_escape("with spaces"), "with spaces");
    }

    #[test]
    fn toml_escape_quotes_and_backslashes_use_basic_escapes() {
        assert_eq!(toml_escape(r#"quote " inside"#), r#"quote \" inside"#);
        assert_eq!(toml_escape(r"back \ slash"), r"back \\ slash");
    }

    #[test]
    fn toml_escape_handles_named_control_chars() {
        assert_eq!(toml_escape("line\nbreak"), r"line\nbreak");
        assert_eq!(toml_escape("tab\there"), r"tab\there");
        assert_eq!(toml_escape("cr\rback"), r"cr\rback");
        assert_eq!(toml_escape("bel\x08"), r"bel\b");
        assert_eq!(toml_escape("ff\x0c"), r"ff\f");
    }

    #[test]
    #[allow(non_snake_case)]
    fn toml_escape_emits_uXXXX_for_unnamed_control_chars() {
        // NUL, ESC, DEL.
        assert_eq!(toml_escape("\0"), "\\u0000");
        assert_eq!(toml_escape("\x1b"), "\\u001B");
        assert_eq!(toml_escape("\x7f"), "\\u007F");
    }

    #[test]
    fn match_needles_in_order_finds_full_sequence_when_ordered() {
        // M4.6 P1: rolling-history matcher must succeed when every
        // needle appears in order, even when two transitions land
        // back-to-back in a single chunk.
        let haystack = b"prelude Thinking... then Working with `Bash` then Idle now";
        let needles = ["Thinking", "Working", "Bash", "Idle"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, needles.len());
    }

    #[test]
    fn match_needles_in_order_stops_when_needle_is_out_of_order() {
        // Sequence: text contains Working before Thinking — the
        // matcher must stop at index 1 (Thinking found, Working
        // already passed by the cursor).
        let haystack = b"Working appears first, then Thinking arrives later";
        let needles = ["Thinking", "Working"];
        let matched = match_needles_in_order(haystack, &needles);
        // Thinking is found (offset > 0). Then we search for Working
        // AFTER Thinking — and there's no second Working, so the
        // match stops at 1.
        assert_eq!(matched, 1);
    }

    #[test]
    fn match_needles_in_order_returns_zero_when_first_needle_missing() {
        // Used by wait_for_strings_in_order's timeout path: if even
        // the first needle never appears, `matched` stays 0 so the
        // panic message points at the right substring.
        let haystack = b"completely unrelated output, no status labels here";
        let needles = ["Thinking", "Working"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, 0);
    }

    #[test]
    fn match_needles_in_order_partial_when_later_needle_missing() {
        // Thinking + Working land in the history, but Bash never
        // shows up — matcher reports 2 (the cursor advanced past
        // both before failing on Bash). wait_for_strings_in_order
        // then surfaces "did not see `Bash` (needle #3 of 4)" on
        // timeout.
        let haystack = b"Thinking happened then Working took over, no tool was used";
        let needles = ["Thinking", "Working", "Bash", "Idle"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, 2);
    }
}
