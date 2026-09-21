//! A PTY-attached deck, driven by keystrokes and read back through a vt100
//! terminal emulator.
//!
//! This is the same mechanism the L2 e2e harness uses (`tests/common/mod.rs`),
//! reimplemented here rather than reused for one reason: that harness launches
//! `env!("CARGO_BIN_EXE_dot-agent-deck")`, the binary the *test build* produced,
//! and links `dot_agent_deck` for its protocol types. A cross-version run needs
//! to drive **two different binaries on disk** — one of which is a published
//! release whose protocol types this workspace does not have — so everything
//! here takes a binary path and reads the deck only through its terminal and
//! its CLI.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::proc;

/// How often a `wait_for_*` re-reads the screen. Short enough that a settled
/// frame is noticed promptly, long enough that a whole run costs a few thousand
/// wakeups rather than a core.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Query the deck emits to detect the enhanced (kitty) keyboard protocol:
/// `ESC [ ? u`. Written by `crossterm::terminal::supports_keyboard_enhancement()`.
const QUERY_KITTY_FLAGS: &[u8] = b"\x1b[?u";
/// The second half of that probe: `ESC [ c` (primary device attributes, DA1).
const QUERY_DA1: &[u8] = b"\x1b[c";
/// Reply to [`QUERY_KITTY_FLAGS`]: `CSI ? 1 u`.
const REPLY_KITTY_FLAGS: &[u8] = b"\x1b[?1u";
/// Reply to [`QUERY_DA1`]: a plain VT220-class DA1 response.
const REPLY_DA1: &[u8] = b"\x1b[?62;22c";
/// Longest query pattern above, in bytes.
const LONGEST_QUERY_LEN: usize = 4;

/// Answer the terminal-capability queries the deck writes to its tty, so its
/// startup probe returns immediately instead of blocking for 2000 ms.
///
/// Ported from `tests/common/mod.rs::answer_terminal_queries`, which carries the
/// full reasoning (PRD #227 M2). Without it every launch here would pay two
/// seconds and come up with the enhanced keyboard protocol disabled, which is
/// not the configuration a user's terminal presents.
fn answer_terminal_queries(chunk: &[u8], scan: &mut Vec<u8>, writer: &mut dyn Write) {
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .filter(|_| !needle.is_empty())
    }

    scan.extend_from_slice(chunk);
    let mut reply: Vec<u8> = Vec::new();
    loop {
        let hit = [
            (
                find(scan, QUERY_KITTY_FLAGS),
                QUERY_KITTY_FLAGS,
                REPLY_KITTY_FLAGS,
            ),
            (find(scan, QUERY_DA1), QUERY_DA1, REPLY_DA1),
        ]
        .into_iter()
        .filter_map(|(pos, q, r)| pos.map(|p| (p, q.len(), r)))
        .min_by_key(|(pos, _, _)| *pos);
        let Some((pos, qlen, r)) = hit else { break };
        reply.extend_from_slice(r);
        scan.drain(..pos + qlen);
    }
    if scan.len() > LONGEST_QUERY_LEN - 1 {
        let cut = scan.len() - (LONGEST_QUERY_LEN - 1);
        scan.drain(..cut);
    }
    if !reply.is_empty() {
        let _ = writer.write_all(&reply);
        let _ = writer.flush();
    }
}

/// A deck running under a pseudo-terminal this process owns both ends of.
pub struct PtyDeck {
    /// Human label used in every failure message and in the evidence file.
    pub label: String,
    /// Where the raw byte stream is mirrored, so a failed run leaves something
    /// a reader who did not watch it can inspect.
    pub stream_log: PathBuf,
    _master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    history: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn Child + Send + Sync>,
    /// Captured right after spawn; re-verified before the one signal this
    /// struct ever sends.
    pub identity: proc::Identity,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    shut_down: bool,
}

/// What to launch under a PTY.
pub struct PtySpec<'a> {
    /// Human label used in every failure message and in the evidence file.
    pub label: &'a str,
    pub bin: &'a Path,
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    /// Applied after an explicit `env_clear`, so the child sees exactly what the
    /// caller listed and nothing from this process — which is what lets a run
    /// pin `XDG_RUNTIME_DIR` absent rather than hope it is.
    pub env: &'a [(String, String)],
    pub cols: u16,
    pub rows: u16,
    /// Where the raw byte stream is mirrored on shutdown.
    pub stream_log: PathBuf,
}

impl PtyDeck {
    /// Spawn the deck described by `spec` under a fresh PTY.
    pub fn spawn(spec: PtySpec<'_>) -> Result<Self, String> {
        let PtySpec {
            label,
            bin,
            args,
            cwd,
            env,
            cols,
            rows,
            stream_log,
        } = spec;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty for {label}: {e}"))?;

        let mut cmd = CommandBuilder::new(bin);
        for a in args {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        cmd.env_clear();
        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn {} for {label}: {e}", bin.display()))?;
        drop(pair.slave);
        // Fail closed: a client whose identity cannot be recorded is one that
        // could not later be signalled safely, so it is not left running.
        let identity = match child
            .process_id()
            .ok_or_else(|| format!("{label}: the PTY child reported no pid"))
            .and_then(|pid| proc::Identity::capture_spawned(pid as i32, bin))
        {
            Ok(id) => id,
            Err(e) => {
                // Still un-reaped, so this handle cannot reach a recycled pid.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{label}: could not record its identity: {e}"));
            }
        };

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let history = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|e| format!("take PTY writer for {label}: {e}"))?,
        ));

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone PTY reader for {label}: {e}"))?;
        let parser_for_reader = Arc::clone(&parser);
        let history_for_reader = Arc::clone(&history);
        let stop_for_reader = Arc::clone(&stop);
        let writer_for_reader = Arc::clone(&writer);
        let handle = std::thread::Builder::new()
            .name(format!("xver-reader-{label}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut query_scan: Vec<u8> = Vec::new();
                while !stop_for_reader.load(Ordering::Relaxed) {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            parser_for_reader.lock().unwrap().process(chunk);
                            history_for_reader.lock().unwrap().extend_from_slice(chunk);
                            answer_terminal_queries(
                                chunk,
                                &mut query_scan,
                                &mut *writer_for_reader.lock().unwrap(),
                            );
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
            .map_err(|e| format!("spawn reader thread for {label}: {e}"))?;

        Ok(Self {
            label: label.to_string(),
            stream_log,
            _master: pair.master,
            writer,
            parser,
            history,
            child,
            identity,
            stop,
            reader: Some(handle),
            shut_down: false,
        })
    }

    /// The rendered screen, rows joined with newlines — what a person looking at
    /// this terminal right now would see.
    pub fn grid(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Every byte the deck has written since launch, lossily decoded.
    ///
    /// Distinct from [`grid`](Self::grid) and needed for anything the deck
    /// printed *before* it took the alternate screen — the build-version
    /// mismatch prompt is exactly that, and the grid has overwritten it by the
    /// time the deck has drawn a frame.
    pub fn stream(&self) -> String {
        String::from_utf8_lossy(&self.history.lock().unwrap()).into_owned()
    }

    /// The stream with ANSI escape sequences removed, so a needle that the deck
    /// painted with styling still matches as plain text.
    pub fn stream_text(&self) -> String {
        strip_ansi(&self.stream())
    }

    pub fn send(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    /// Wait until the rendered screen satisfies `pred`. Returns whether it did.
    pub fn wait_for_grid(&self, timeout: Duration, pred: impl Fn(&str) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.grid()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn wait_for_grid_string(&self, needle: &str, timeout: Duration) -> bool {
        self.wait_for_grid(timeout, |g| g.contains(needle))
    }

    /// Wait until `needle` has appeared anywhere in the byte history.
    pub fn wait_for_stream_string(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.stream_text().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Wait for the deck process to exit. `None` means it was still running at
    /// the deadline.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.success()),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Persist the raw stream, then stop the child if it is still running and
    /// stop the reader thread. Returns what happened, for the run log.
    ///
    /// The one signal here goes to the one child this struct spawned, through
    /// its still-un-reaped handle, and only after its whole recorded identity
    /// has been re-read and matched — never a pattern match. See
    /// `docs/develop/cross-version-harness.md`'s teardown section for why that
    /// distinction is load-bearing. Idempotent.
    pub fn shutdown(&mut self) -> String {
        if self.shut_down {
            return format!("{}: already shut down", self.label);
        }
        self.shut_down = true;
        if let Some(parent) = self.stream_log.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.stream_log, self.stream().as_bytes());
        let note = match self.child.try_wait() {
            Ok(Some(status)) => format!("{} had already exited ({status:?})", self.label),
            _ => match self.identity.verify() {
                Ok(()) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    format!(
                        "{} (pid {}) stopped through its un-reaped child handle after its full \
                         identity was re-verified",
                        self.label, self.identity.pid
                    )
                }
                Err(proc::Mismatch::Gone) => {
                    let _ = self.child.wait();
                    format!(
                        "{} (pid {}) was already gone",
                        self.label, self.identity.pid
                    )
                }
                Err(m) => {
                    // Refused. Do not join the reader either: with the child
                    // still holding the PTY it would block forever. The
                    // namespace's exit reaps whatever this was.
                    return format!(
                        "REFUSED to signal {} (pid {}): {m}",
                        self.label, self.identity.pid
                    );
                }
            },
        };
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        note
    }
}

impl Drop for PtyDeck {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Remove ANSI escape sequences from `s`.
///
/// Deliberately coarse: it drops CSI/OSC/single-character escapes and keeps
/// everything else, which is all a substring assertion needs. It is not a
/// terminal emulator — [`PtyDeck::grid`] is, and that is what any assertion
/// about *layout* should read.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                // CSI: parameters/intermediates, then a final byte in 0x40..=0x7e.
                b'[' => {
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                // OSC: runs to BEL or ST.
                b']' => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                _ => i += 1,
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_and_keeps_text() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m text"), "red text");
    }

    #[test]
    fn strip_ansi_removes_osc_terminated_by_bel_or_st() {
        assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\after"), "after");
    }

    #[test]
    fn strip_ansi_keeps_newlines_so_line_needles_still_match() {
        assert_eq!(strip_ansi("a\r\n\x1b[Kb"), "a\r\nb");
    }

    #[test]
    fn answer_terminal_queries_replies_to_both_halves_of_the_probe() {
        let mut scan = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        answer_terminal_queries(b"\x1b[?u\x1b[c", &mut scan, &mut out);
        assert_eq!(out, [REPLY_KITTY_FLAGS, REPLY_DA1].concat());
    }

    #[test]
    fn answer_terminal_queries_matches_a_probe_split_across_two_reads() {
        let mut scan = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        answer_terminal_queries(b"noise\x1b[?", &mut scan, &mut out);
        assert!(out.is_empty(), "half a query must not be answered");
        answer_terminal_queries(b"u", &mut scan, &mut out);
        assert_eq!(out, REPLY_KITTY_FLAGS);
    }
}
