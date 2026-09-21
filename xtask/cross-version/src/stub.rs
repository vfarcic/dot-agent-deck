//! A synthetic Claude Code stand-in for the reverse probes that need one.
//!
//! It is this harness's own binary, hard-linked into the sandbox as
//! `$S/stub/claude`: the deck types an agent as Claude Code from its command's
//! basename (`AgentType::from_command`), and two changed arms — #1031's
//! late-readiness recovery and #1182's paste-envelope matcher — are only
//! reachable for a Claude-typed pane. `main.rs` dispatches here when `argv[0]`'s
//! basename is `claude`.
//!
//! It is deterministic where the real agent is not, and deliberately models only
//! the ONE producer behaviour each probe is about:
//!
//! * `--xver-mode=late-boot` (#1031): swallows the first submit CR it receives
//!   without acting on it — the pointer stays in its input, as #1031 measured a
//!   real booting Claude doing — and submits on every later CR. It sends NO hook
//!   event on its own; when the harness creates its trigger file it runs the old
//!   hook CLI once with a late `SessionStart`.
//! * `--xver-mode=paste-envelope` (#1182): announces itself with a
//!   `SessionStart` a second after it starts, captures each bracketed paste
//!   exactly, and on the submit CR that follows one reports it through the old
//!   hook CLI inside Claude's `<pasted_content id="57b9">` envelope.
//!
//! Every hook call is made from THIS process, with the environment the daemon
//! gave its pane, so each carries the pane's genuine identity. Everything it
//! does is appended to `$S/artifacts/stub-<mode>.log`, one line each, prefixed
//! with its pid — a respawned incarnation is a different pid.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::sandbox::Sandbox;

pub const SWALLOWED: &str = "SWALLOW-STUB-SWALLOWED:";
pub const SUBMITTED: &str = "SWALLOW-STUB-SUBMITTED:";
pub const HOOK_RAN: &str = "HOOK-RAN";
pub const PASTE_CAPTURED: &str = "PASTE-CAPTURED";

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    LateBoot,
    PasteEnvelope,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s {
            "late-boot" => Some(Mode::LateBoot),
            "paste-envelope" => Some(Mode::PasteEnvelope),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::LateBoot => "late-boot",
            Mode::PasteEnvelope => "paste-envelope",
        }
    }
}

pub fn log_path(sb: &Sandbox, mode: Mode) -> PathBuf {
    sb.artifacts.join(format!("stub-{}.log", mode.name()))
}

fn trigger_path(sb: &Sandbox, mode: Mode) -> PathBuf {
    sb.artifacts.join(format!("stub-{}.trigger", mode.name()))
}

/// Ask a running stub to fire its one triggered hook call.
pub fn trigger(sb: &Sandbox, mode: Mode) -> Result<(), String> {
    let p = trigger_path(sb, mode);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .map(|_| ())
        .map_err(|e| format!("create the stub trigger {}: {e}", p.display()))
}

/// The keystroke stream a pane's program receives, split into what a composer
/// would hold (`line`), bracketed pastes (`paste`), and submits.
#[derive(Default)]
pub struct Input {
    pending: Vec<u8>,
    in_paste: bool,
    paste: Vec<u8>,
    /// The last COMPLETE paste since the last submit.
    pub last_paste: Option<Vec<u8>>,
    /// Bytes typed outside a paste since the last cleared submit.
    pub line: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputEvent {
    PasteComplete(usize),
    Submit,
}

impl Input {
    /// Consume `bytes`, returning what happened. A marker split across two
    /// reads is held until it completes.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let rest = &self.pending[i..];
            if rest[0] == 0x1b {
                if rest.starts_with(PASTE_START) {
                    self.in_paste = true;
                    self.paste.clear();
                    i += PASTE_START.len();
                    continue;
                }
                if rest.starts_with(PASTE_END) {
                    self.in_paste = false;
                    let p = std::mem::take(&mut self.paste);
                    out.push(InputEvent::PasteComplete(p.len()));
                    self.last_paste = Some(p);
                    i += PASTE_END.len();
                    continue;
                }
                if PASTE_START.starts_with(rest) || PASTE_END.starts_with(rest) {
                    break;
                }
            }
            let b = rest[0];
            i += 1;
            if self.in_paste {
                self.paste.push(b);
            } else if b == b'\r' {
                out.push(InputEvent::Submit);
            } else {
                self.line.push(b);
            }
        }
        self.pending.drain(..i);
        out
    }
}

/// The prompt Claude Code reports for a submitted bracketed paste: the payload
/// wrapped in its envelope, as #1182 measured it.
pub fn envelope(payload: &str) -> String {
    format!("\n\n<pasted_content id=\"57b9\">\n{payload}\n</pasted_content id=\"57b9\">")
}

struct Log {
    path: PathBuf,
    pid: u32,
}

impl Log {
    fn line(&self, msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{} {}", self.pid, msg.replace(['\r', '\n'], "⏎"));
        }
    }
}

fn say(msg: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(msg.as_bytes());
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
}

/// Put fd 0 into raw mode — what a real agent's TUI does — and restore it on
/// drop.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> Self {
        // SAFETY: `termios` is plain data; `tcgetattr`/`tcsetattr` only read
        // and write it, on fd 0, and fail harmlessly when fd 0 is not a tty.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return RawMode(None);
            }
            RawMode(Some(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restoring the attributes read in `enable`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        }
    }
}

/// Wait up to `timeout` for fd 0 to be readable; `Some(bytes)` (empty on EOF)
/// or `None` on timeout.
fn read_some(timeout: Duration) -> Option<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call.
    let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    if n <= 0 {
        return None;
    }
    let mut buf = [0u8; 4096];
    match std::io::stdin().lock().read(&mut buf) {
        Ok(k) => Some(buf[..k].to_vec()),
        Err(_) => Some(Vec::new()),
    }
}

/// Run the previous release's `hook --agent claude-code` with `payload` on its
/// stdin, from this process, with this process's environment — the pane's.
fn run_hook(old: &Path, payload: &serde_json::Value, what: &str, log: &Log) {
    let spawned = Command::new(old)
        .args(["hook", "--agent", "claude-code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            log.line(&format!("{HOOK_RAN} {what} spawn-failed {e}"));
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break format!("{s}"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break "timed out after 10s and was killed".to_string();
            }
            Err(e) => break format!("wait failed: {e}"),
        }
    };
    log.line(&format!("{HOOK_RAN} {what} {status}"));
}

/// Entry point when the binary runs as `claude`.
pub fn main() -> ExitCode {
    let mode = std::env::args()
        .skip(1)
        .find_map(|a| a.strip_prefix("--xver-mode=").and_then(Mode::parse));
    let Some(mode) = mode else {
        eprintln!("xver stub: no --xver-mode=late-boot|paste-envelope argument");
        return ExitCode::FAILURE;
    };
    let Some(root) = std::env::var_os("DAD_XVER_SANDBOX") else {
        eprintln!("xver stub: DAD_XVER_SANDBOX is not set — this runs only inside a run");
        return ExitCode::FAILURE;
    };
    let sb = Sandbox::at(PathBuf::from(root));
    let old = sb.old_bin();
    let log = Log {
        path: log_path(&sb, mode),
        pid: std::process::id(),
    };
    log.line(&format!(
        "START mode={} pane={} agent={}",
        mode.name(),
        std::env::var("DOT_AGENT_DECK_PANE_ID").unwrap_or_default(),
        std::env::var("DOT_AGENT_DECK_AGENT_ID").unwrap_or_default()
    ));
    let _raw = RawMode::enable();
    say(&format!(
        "XVER_STUB_{}_READY",
        mode.name().to_ascii_uppercase().replace('-', "_")
    ));
    let session = match mode {
        Mode::LateBoot => "xver-1031-late",
        Mode::PasteEnvelope => "xver-1182-rev",
    };
    let started = Instant::now();
    let mut announced = false;
    let mut swallowed_once = false;
    let mut input = Input::default();
    loop {
        if mode == Mode::PasteEnvelope && !announced && started.elapsed() >= Duration::from_secs(1)
        {
            announced = true;
            run_hook(
                &old,
                &serde_json::json!({
                    "hook_event_name": "SessionStart",
                    "session_id": session,
                    "source": "startup",
                }),
                "SessionStart(startup, at boot)",
                &log,
            );
        }
        if mode == Mode::LateBoot {
            let t = trigger_path(&sb, mode);
            let consumed = t.with_extension("consumed");
            if t.exists() && std::fs::rename(&t, &consumed).is_ok() {
                run_hook(
                    &old,
                    &serde_json::json!({
                        "hook_event_name": "SessionStart",
                        "session_id": session,
                        "source": "startup",
                    }),
                    "SessionStart(startup, late)",
                    &log,
                );
            }
        }
        let Some(bytes) = read_some(Duration::from_millis(100)) else {
            continue;
        };
        if bytes.is_empty() {
            log.line("EOF on the pane — exiting");
            return ExitCode::SUCCESS;
        }
        for ev in input.feed(&bytes) {
            match (mode, ev) {
                (Mode::PasteEnvelope, InputEvent::PasteComplete(n)) => {
                    log.line(&format!("{PASTE_CAPTURED} {n} bytes"));
                }
                (Mode::PasteEnvelope, InputEvent::Submit) => {
                    let Some(paste) = input.last_paste.take() else {
                        log.line("SUBMIT with no paste — ignored");
                        continue;
                    };
                    let payload = String::from_utf8_lossy(&paste).into_owned();
                    say(&format!("PASTE-STUB-SUBMITTED {} bytes", paste.len()));
                    run_hook(
                        &old,
                        &serde_json::json!({
                            "hook_event_name": "UserPromptSubmit",
                            "session_id": session,
                            "prompt": envelope(&payload),
                        }),
                        "UserPromptSubmit(pasted_content envelope)",
                        &log,
                    );
                    input.line.clear();
                }
                (Mode::LateBoot, InputEvent::Submit) => {
                    let line = String::from_utf8_lossy(&input.line).into_owned();
                    if swallowed_once {
                        let msg = format!("{SUBMITTED}{line}");
                        say(&msg);
                        log.line(&msg);
                        input.line.clear();
                    } else {
                        swallowed_once = true;
                        let msg = format!("{SWALLOWED}{line}");
                        say(&msg);
                        log.line(&msg);
                    }
                }
                (Mode::LateBoot, InputEvent::PasteComplete(n)) => {
                    log.line(&format!("{PASTE_CAPTURED} {n} bytes (unexpected here)"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bracketed_paste_is_captured_exactly_and_the_cr_after_it_submits() {
        let mut i = Input::default();
        let ev = i.feed(b"\x1b[200~line one\nline two\x1b[201~");
        assert_eq!(ev, vec![InputEvent::PasteComplete(17)]);
        assert_eq!(i.last_paste.as_deref(), Some(&b"line one\nline two"[..]));
        assert_eq!(i.feed(b"\r"), vec![InputEvent::Submit]);
        assert!(
            i.line.is_empty(),
            "paste bytes never leak into the composer line"
        );
    }

    #[test]
    fn a_marker_split_across_two_reads_is_still_recognised() {
        let mut i = Input::default();
        assert!(i.feed(b"\x1b[20").is_empty());
        assert!(i.feed(b"0~abc\x1b[2").is_empty());
        assert_eq!(
            i.feed(b"01~\r"),
            vec![InputEvent::PasteComplete(3), InputEvent::Submit]
        );
        assert_eq!(i.last_paste.as_deref(), Some(&b"abc"[..]));
    }

    #[test]
    fn a_raw_pointer_then_cr_lands_in_the_composer_line() {
        let mut i = Input::default();
        let ev = i.feed(b"Read .dot-agent-deck/worker-task-lateboot.md for your task.\r");
        assert_eq!(ev, vec![InputEvent::Submit]);
        assert_eq!(
            i.line,
            b"Read .dot-agent-deck/worker-task-lateboot.md for your task."
        );
    }

    #[test]
    fn the_envelope_is_claudes_measured_shape() {
        assert_eq!(
            envelope("a\nb"),
            "\n\n<pasted_content id=\"57b9\">\na\nb\n</pasted_content id=\"57b9\">"
        );
    }
}
