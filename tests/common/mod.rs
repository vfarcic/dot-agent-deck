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

/// Which agent's credential set the test wants imported from the host
/// HOME into the per-test tempdir HOME. Both variants are real
/// `std::fs::copy` (M2.1 auditor S3 — no symlinks across fixtures);
/// the credentials file is re-stamped to 0o600 after copy.
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
    /// per-test paths. Real `fs::copy` (no symlinks per M2.1 auditor
    /// S3); 0o600 is preserved on the credentials file.
    ///
    /// The actual copy happens at launch time; if a required source
    /// file is missing the launch panics with the per-Decision-26
    /// skip message. Tests should pair this with
    /// [`check_claude_available`] and [`skip_unless!`] to convert that
    /// panic into a clean runtime skip.
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

    /// Launch the deck against the named fixture under
    /// `tests/fixtures/`. The fixture is copied into the per-test
    /// tempdir at launch (Decision 12); the deck's `HOME`, hook socket,
    /// and attach socket all point inside that tempdir.
    pub fn launch_with_fixture(self, fixture_name: &str) -> TuiDeck {
        TuiDeck::launch_inner(self, fixture_name)
    }
}

/// Handle to a running deck.
pub struct TuiDeck {
    pty_master: Box<dyn MasterPty + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_byte_at: Arc<Mutex<Instant>>,
    cast_events: Arc<Mutex<Vec<CastEvent>>>,
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
        }
    }

    fn launch_inner(builder: TuiDeckBuilder, fixture_name: &str) -> Self {
        let test_name = current_test_name();

        let tempdir = tempfile::tempdir().expect("create per-test tempdir");
        let work = tempdir.path().to_path_buf();

        // M2.1 auditor S1: lock the per-test tempdir to 0o700 so a
        // co-located uid on a shared developer machine cannot list
        // per-test HOME contents, fixtures, or failure recordings.
        // The sockets themselves are already 0o600; the parent dir
        // needs to match.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&work, perms).expect("set tempdir mode to 0o700");
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

        // Chain-smoke credential imports (PRD #77 Decision 8). Tests
        // pair these with `check_*_available()` + `skip_unless!`; if
        // the credentials disappeared between the precheck and here,
        // we panic with the Decision-26-shaped skip message so the
        // failure surfaces explicitly rather than running a logged-out
        // CLI that would burn the test budget on a 401 storm.
        for kind in &builder.credential_imports {
            match kind {
                CredentialImport::ClaudeCode => {
                    import_claude_credentials(&home).expect("import Claude credentials");
                }
                CredentialImport::OpenCode => {
                    import_opencode_credentials(&home).expect("import OpenCode credentials");
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
        let reader_stop = Arc::new(AtomicBool::new(false));
        let cast_started_at = Instant::now();

        // Reader thread: pulls bytes off the PTY master, feeds the
        // parser, updates `last_byte_at`, and appends to the cast log.
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let parser_for_reader = Arc::clone(&parser);
        let last_for_reader = Arc::clone(&last_byte_at);
        let cast_for_reader = Arc::clone(&cast_events);
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

        TuiDeck {
            pty_master: pair.master,
            parser,
            last_byte_at,
            cast_events,
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
        }
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

    /// Returns the deck's per-test hook socket path. Synthetic-event
    /// L2 tests connect to this directly to inject hook payloads.
    pub fn hook_socket_path(&self) -> &Path {
        &self.hook_socket
    }

    /// Returns the deck's per-test attach socket path.
    pub fn attach_socket_path(&self) -> &Path {
        &self.attach_socket
    }

    /// Return the parsed grid contents — used by `wait_for_string`
    /// internally and by tests that want to assert on full-screen
    /// state.
    pub fn snapshot_grid(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
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
        let _ = self.child.kill();
        let _ = self.child.wait();

        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }

        if should_dump {
            // M2.1 auditor S4 + S5: scope to a per-run subdir under an
            // ABSOLUTE workspace-relative path so (a) tests whose cwd
            // is a per-test tempdir still land artifacts in the
            // workspace's target/, and (b) two concurrent
            // `cargo test-e2e` invocations cannot clobber each
            // other's artifacts.
            let recordings_dir = workspace_test_recordings_root()
                .join(current_run_id())
                .join(sanitize_test_name(&self.test_name));
            if let Err(e) = self.dump_recordings(&recordings_dir) {
                eprintln!("[tui-harness] failed to write recordings to {recordings_dir:?}: {e}");
            }
        }
    }
}

impl TuiDeck {
    fn dump_recordings(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;

        // final-grid.txt
        let grid = self.snapshot_grid();
        std::fs::write(dir.join("final-grid.txt"), &grid)?;

        // final-grid.svg — minimal monospace render. Not pixel-perfect,
        // but valid SVG that opens in any browser.
        let svg = render_grid_to_svg(&grid, self.cols, self.rows);
        std::fs::write(dir.join("final-grid.svg"), svg)?;

        // full-stream.cast — asciinema v2 format (header + one JSON
        // array per event). Inline encoder, ~20 lines.
        let cast = self.encode_asciinema_cast();
        std::fs::write(dir.join("full-stream.cast"), cast)?;

        // fixture.toml — copy of the deck's .dot-agent-deck.toml so a
        // reviewer can replay against the same config.
        let fixture_src = self.fixture_path.join(".dot-agent-deck.toml");
        if fixture_src.exists() {
            std::fs::copy(&fixture_src, dir.join("fixture.toml"))?;
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
// Helpers
// ---------------------------------------------------------------------------

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

/// Workspace-relative `target/test-recordings/` resolved to an
/// ABSOLUTE path at harness construction time. The fixture-copy step
/// `cwd`s the deck into a per-test tempdir, so any relative path here
/// would land artifacts in the wrong place (M2.1 auditor S5).
fn workspace_test_recordings_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-recordings")
}

/// Identifier for the active `cargo test-e2e` invocation. Nextest
/// sets `NEXTEST_RUN_ID` for setup scripts and tests; outside nextest
/// we fall back to a process-PID + nanosecond-timestamp combination
/// that two concurrent invocations cannot collide on. Used by the
/// recordings dump (M2.1 auditor S4).
fn current_run_id() -> String {
    if let Ok(id) = std::env::var("NEXTEST_RUN_ID")
        && !id.is_empty()
    {
        return id;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("run-{}-{nanos}", std::process::id())
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
    if std::process::Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        .not()
    {
        return Err("Claude Code CLI not installed (could not invoke `claude --version`)".into());
    }
    let home = host_home();
    let creds = home.join(".claude").join(".credentials.json");
    if !creds.exists() {
        return Err(format!(
            "Claude Code credentials not found at {} — log in with `claude login`",
            creds.display()
        ));
    }
    Ok(())
}

/// PRD #77 Decision 26 runtime-skip helper for OpenCode. Mirrors
/// [`check_claude_available`] — checks for the CLI on PATH and an
/// OpenCode auth.json (or analogous credential the user logged in
/// with).
pub fn check_opencode_available() -> Result<(), String> {
    if std::process::Command::new("opencode")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        .not()
    {
        return Err("OpenCode CLI not installed (could not invoke `opencode --version`)".into());
    }
    let home = host_home();
    // OpenCode stores creds under the data dir. Per the PRD task spec
    // we check the legacy / new shapes the M2.1 + M3 audit identified;
    // first found wins. If neither is present, surface the canonical
    // path so the operator knows where to log in.
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
    Err(format!(
        "OpenCode credentials not found at {} — log in with `opencode auth login`",
        candidates[0].display()
    ))
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

trait BoolNot {
    fn not(self) -> Self;
}
impl BoolNot for bool {
    fn not(self) -> bool {
        !self
    }
}

/// Copy the host user's Claude Code credentials + settings into the
/// per-test tempdir HOME. Strips any `hooks` entries from the
/// imported `settings.json` (the deck auto-installs its own hooks
/// pointing at the per-test socket — leaving the host's hook entries
/// in place would invoke the developer's real hook commands inside
/// the test). Re-stamps the credentials file mode to 0o600 after
/// copy.
fn import_claude_credentials(test_home: &Path) -> std::io::Result<()> {
    let src_root = host_home().join(".claude");
    let dst_root = test_home.join(".claude");
    std::fs::create_dir_all(&dst_root)?;

    let src_creds = src_root.join(".credentials.json");
    if !src_creds.is_file() {
        return Err(std::io::Error::other(format!(
            "Claude Code credentials not found at {} — log in with `claude login`",
            src_creds.display()
        )));
    }
    let dst_creds = dst_root.join(".credentials.json");
    std::fs::copy(&src_creds, &dst_creds)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst_creds, std::fs::Permissions::from_mode(0o600))?;
    }

    // settings.json: copy if present, with `hooks` stripped.
    let src_settings = src_root.join("settings.json");
    if src_settings.is_file() {
        let raw = std::fs::read_to_string(&src_settings)?;
        let dst_text = strip_hooks_from_claude_settings(&raw);
        std::fs::write(dst_root.join("settings.json"), dst_text)?;
    }

    // plugins/ (and any other supporting dirs) — best-effort copy if
    // present. `copy_dir_recursively` refuses symlinks (M2.1 auditor
    // Nit 3); the user's plugins dir should be regular files / dirs.
    let src_plugins = src_root.join("plugins");
    if src_plugins.is_dir() {
        copy_dir_recursively(&src_plugins, &dst_root.join("plugins"))?;
    }
    Ok(())
}

/// Strip the top-level `hooks` key from a Claude Code settings.json.
/// Best-effort textual edit when the file is valid JSON; on parse
/// failure we fall back to returning the original (worst case, the
/// host's hooks fire inside the test).
fn strip_hooks_from_claude_settings(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("hooks");
            }
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string())
        }
        Err(_) => raw.to_string(),
    }
}

/// Copy the host user's OpenCode credentials into the per-test
/// tempdir HOME. Mirrors [`import_claude_credentials`] — copies the
/// auth state but NOT any `plugin/` directory (the deck installs its
/// own OpenCode plugin pointing at the per-test paths). Touches
/// whichever of the legacy / new credential paths exist on the host.
fn import_opencode_credentials(test_home: &Path) -> std::io::Result<()> {
    let mut imported_any = false;

    // Possible source roots for the auth state. The first directory
    // that exists is copied wholesale (sans `plugin/`); if none
    // exists we surface the canonical path.
    let source_roots = [
        host_home().join(".local").join("share").join("opencode"),
        host_home().join(".opencode"),
    ];
    for src in &source_roots {
        if src.is_dir() {
            let rel = src
                .strip_prefix(host_home())
                .expect("HOME-relative source path");
            let dst = test_home.join(rel);
            copy_dir_excluding_plugin_subdir(src, &dst)?;
            imported_any = true;
        }
    }

    // ~/.config/opencode/opencode.jsonc is the user-editable config.
    let src_cfg = host_home()
        .join(".config")
        .join("opencode")
        .join("opencode.jsonc");
    if src_cfg.is_file() {
        let dst_cfg_dir = test_home.join(".config").join("opencode");
        std::fs::create_dir_all(&dst_cfg_dir)?;
        std::fs::copy(&src_cfg, dst_cfg_dir.join("opencode.jsonc"))?;
        imported_any = true;
    }

    if !imported_any {
        return Err(std::io::Error::other(format!(
            "OpenCode credentials not found under {} — log in with `opencode auth login`",
            source_roots[0].display()
        )));
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
            #[cfg(unix)]
            if entry.file_name() == "auth.json" {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
            }
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

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
