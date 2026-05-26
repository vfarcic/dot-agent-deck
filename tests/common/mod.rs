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

/// Builder for [`TuiDeck`]. Use the test surface
/// [`TuiDeck::builder`].
pub struct TuiDeckBuilder {
    cols: u16,
    rows: u16,
    extra_env: Vec<(String, String)>,
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
        }
    }

    fn launch_inner(builder: TuiDeckBuilder, fixture_name: &str) -> Self {
        let test_name = current_test_name();

        let tempdir = tempfile::tempdir().expect("create per-test tempdir");
        let work = tempdir.path().to_path_buf();

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

        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = CommandBuilder::new(bin);
        cmd.cwd(&work);

        // Decision 20: scrub host inheritance, set pinned values, then
        // layer the test's per-deck env (so a test asking for `NO_COLOR=1`
        // wins). The harness controls the entire env passed to portable-pty
        // — we never pass `cmd.env_clear` (it surfaces inconsistently
        // across platforms), instead we set every var explicitly.
        let state_dir = work.join("state");
        let pinned: &[(&str, &str)] = &[
            ("TERM", "xterm-256color"),
            ("LC_ALL", "C.UTF-8"),
            ("COLORTERM", "truecolor"),
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
            let recordings_dir = std::path::PathBuf::from("target/test-recordings")
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
        }
    }
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
