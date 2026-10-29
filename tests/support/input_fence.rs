//! Native raw-terminal fixture for S-15/S-16. Mailbox commands are fixture controls,
//! never production transport shortcuts. All input assertions read PTY bytes.
//!
//! Assumed Q35-15 interface: the existing generation-bound, e2e-only
//! `e2e-composer-control` accepts `fence`, `release`, and read-only `fence-status`.
//! `fence` closes the production gate at the current output offset without
//! writing input. `release` uses the production drain/revalidation path.
//! `fence-status` returns `composer_fence: {held_ops, full_waiters}`; these
//! counts only arrange races, never stand in for the terminal-byte oracle.
//! PaneWriter::accept(op, ticket) is not called by these tests: Accepted must
//! let the attach task read again, Full must park that one connection without
//! holding the writer lock, and Open must await the actual physical outcome.

use crate::{common, inbox};
use dot_agent_deck::daemon_protocol::{
    KIND_REQ, KIND_RESP, KIND_STREAM_END, KIND_STREAM_IN, KIND_STREAM_OUT, KIND_STREAM_REJECT,
};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const CAP: usize = 1024 * 1024;
pub const REPLY: &[u8] = b"\x1b[?62;22c";
const QUERY: &[u8] = b"\x1b[c";
const ENTRY: &str = "probe_015_open_attach_keeps_bytes_detach_resize_and_empty_frames";
const MODE: &str = "DAD_INPUT_FENCE_AGENT";
const PANE: &str = "input-fence-recorder";

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn atomic_json(path: &Path, value: &Value) {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temporary
        .write_all(&serde_json::to_vec(value).unwrap())
        .unwrap();
    temporary.persist(path).unwrap();
}

fn read_json(path: &Path) -> Value {
    assert!(
        common::wait_until(common::child_boot_budget(), || path.exists()),
        "native recorder acknowledgement missing: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

pub struct Fixture {
    daemon: common::DaemonProc,
    base: PathBuf,
    exe: PathBuf,
    id: String,
    generation: u64,
    previous: Option<(PathBuf, u64)>,
    installed: bool,
}

impl Fixture {
    pub fn start(installed: bool) -> Self {
        let extra = if installed {
            vec![("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", "claude@2.1.310")]
        } else {
            vec![]
        };
        let daemon = common::spawn_daemon_serve_with_env(None, "0", &extra);
        let native = daemon.home.join("native");
        fs::create_dir(&native).unwrap();
        let exe = native.join("claude");
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let base = daemon.home.join("recorder-first");
        let mut f = Self {
            daemon,
            base,
            exe,
            id: String::new(),
            generation: 0,
            previous: None,
            installed,
        };
        f.spawn();
        f
    }

    fn spawn(&mut self) {
        let started = inbox::attach(
            &self.daemon,
            json!({"op": "start-agent",
                "command": format!("{} --exact {ENTRY} --nocapture --test-threads=1", quote(&self.exe)),
                "cwd": self.daemon.home, "rows": 40, "cols": 140,
                "env": [[MODE, self.base.to_str().unwrap()], ["DOT_AGENT_DECK_PANE_ID", PANE]],
            }),
        );
        assert_eq!(started["ok"], true, "native recorder spawn: {started}");
        self.id = started["id"].as_str().unwrap().into();
        let ready = read_json(&self.base.with_extension("ready"));
        assert_eq!(
            ready["exe"],
            json!(self.exe),
            "exec readiness, not just fork"
        );
        self.generation = ready["generation"].as_u64().unwrap();
        if self.installed {
            assert_ne!(
                self.generation, 0,
                "the fence fixture has an installed generation"
            );
        }
    }

    pub fn replace(&mut self) {
        self.previous = Some((self.base.clone(), self.generation));
        self.base = self.daemon.home.join("recorder-successor");
        self.spawn();
        if self.installed {
            assert_ne!(self.generation, self.previous.as_ref().unwrap().1);
        }
    }

    fn control(&self, generation: u64, operation: &str) -> Value {
        inbox::attach(
            &self.daemon,
            json!({"op": "e2e-composer-control", "pane_id": PANE,
            "generation": generation, "operation": operation}),
        )
    }

    fn require_control(&self, operation: &str) -> Value {
        let reply = self.control(self.generation, operation);
        assert_eq!(
            reply["ok"], true,
            "Q35-15 requires the generation-bound {operation} operation: {reply}"
        );
        reply
    }

    pub fn fence(&self) {
        self.require_control("fence");
    }

    /// Q35-16 cuts when a segment is open OR a liability is present. Use
    /// Q35-14b's existing publication control to establish that second
    /// condition, rather than assuming that closing a gate arms a probe.
    pub fn check(&self, client: &Client) {
        self.emit(client, b"\x1b[2J\x1b[H\x1b[2;12HClaude Code v2.1.310\r\n");
        self.require_control("publish");
        self.fence();
    }
    pub fn release(&self) {
        self.require_control("release");
    }
    pub fn old_release(&self) -> Value {
        self.control(self.previous.as_ref().unwrap().1, "release")
    }

    pub fn wait_held(&self, held: u64, full: u64) {
        let last = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                *last.borrow_mut() = self.require_control("fence-status");
                let last = last.borrow();
                last["composer_fence"]["held_ops"] == held
                    && last["composer_fence"]["full_waiters"] == full
            }),
            "acceptance barrier expected {held} held ops and {full} Full waiters: {last:?}"
        );
    }

    /// A frame may become several lazy sub-ops at a candidate. Wait for
    /// acceptance without prescribing that implementation's node count.
    pub fn wait_any_held(&self) {
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                let status = self.require_control("fence-status");
                status["composer_fence"]["held_ops"].as_u64().unwrap() > 0
                    && status["composer_fence"]["full_waiters"] == 0
            }),
            "the combined frame must be accepted behind the fence"
        );
    }

    pub fn connect(&self) -> Client {
        Client::attach(&self.daemon.attach_socket, &self.id)
    }

    pub fn command(&self, command: &str) -> Value {
        self.command_value(&json!(command))
    }

    fn command_value(&self, command: &Value) -> Value {
        let response = self.base.with_extension("response");
        assert!(!response.exists());
        atomic_json(&self.base.with_extension("command"), command);
        let result = read_json(&response);
        fs::remove_file(response).unwrap();
        result
    }

    pub fn wait_exit(&self) {
        let ready = read_json(&self.base.with_extension("ready"));
        let proc_dir = PathBuf::from(format!("/proc/{}", ready["pid"].as_u64().unwrap()));
        assert!(
            common::wait_until(common::child_boot_budget(), || !proc_dir.exists()),
            "the fixture's acknowledged native child must exit before replacement"
        );
    }

    fn input(&self) -> Vec<u8> {
        fs::read(self.base.with_extension("input")).unwrap()
    }

    pub fn previous_input(&self) -> Vec<u8> {
        fs::read(self.previous.as_ref().unwrap().0.with_extension("input")).unwrap()
    }

    pub fn expect_input(&self, expected: &[u8]) {
        let matched = common::wait_until(common::child_boot_budget(), || self.input() == expected);
        let actual = self.input();
        assert!(
            matched,
            "raw PTY capture differs: expected {} bytes, got {} bytes; prefix {:?}",
            expected.len(),
            actual.len(),
            &actual[..actual.len().min(80)]
        );
        assert_eq!(actual, expected);
    }

    pub fn expect_stable_input(&self, expected: &[u8]) {
        // A fixture-side stdin drain plus a stability window catches duplicates
        // and writes that incorrectly escaped a fence, not just a late reader.
        self.command("drain");
        assert!(
            !common::wait_until(Duration::from_millis(200), || self.input() != expected),
            "held/settled input changed at the raw terminal: expected {} bytes, got {} bytes",
            expected.len(),
            self.input().len()
        );
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let reply = inbox::attach(
            &self.daemon,
            json!({"op": "resize", "id": self.id, "rows": rows, "cols": cols}),
        );
        assert_eq!(reply["ok"], true, "real terminal resize: {reply}");
    }

    pub fn query(&self, client: &Client) {
        self.command("query");
        assert!(
            common::wait_until(common::child_boot_budget(), || client
                .output()
                .windows(QUERY.len())
                .any(|w| w == QUERY)),
            "query must cross output publication before its reply is sent"
        );
    }

    /// Output originates in the stand-in, crosses the real reader/ledger,
    /// and is observed on attach before the test sends its next input.
    pub fn emit(&self, client: &Client, bytes: &[u8]) {
        assert!(!bytes.is_empty());
        let before = client.output().len();
        self.command_value(&json!({"emit": bytes}));
        assert!(
            common::wait_until(common::child_boot_budget(), || client.output()[before..]
                .windows(bytes.len())
                .any(|window| window == bytes)),
            "fixture output must cross publication before the next acceptance: {bytes:?}"
        );
    }

    /// Read by the stand-in through the same public arm-page ABI as the
    /// installed helper, rather than inspecting the daemon's private FIFO.
    pub fn native_seq(&self) -> u64 {
        let publication = self.command("arm");
        publication["native_seq"].as_u64().unwrap()
    }
}

#[derive(Default)]
struct Received {
    output: Vec<u8>,
    ended: bool,
    refusal: Option<Vec<u8>>,
}

pub struct Client {
    socket: UnixStream,
    received: Arc<Mutex<Received>>,
    reader: Option<JoinHandle<()>>,
}

fn frame(socket: &mut UnixStream, kind: u8, bytes: &[u8]) -> std::io::Result<()> {
    socket.write_all(&[kind])?;
    socket.write_all(&(bytes.len() as u32).to_be_bytes())?;
    socket.write_all(bytes)
}

fn read_frame(socket: &mut UnixStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    socket.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
    assert!(len <= 16 * 1024 * 1024);
    let mut body = vec![0; len];
    socket.read_exact(&mut body)?;
    Ok((header[0], body))
}

impl Client {
    fn attach(path: &Path, id: &str) -> Self {
        let mut socket = UnixStream::connect(path).unwrap();
        socket
            .set_read_timeout(Some(common::child_boot_budget()))
            .unwrap();
        socket
            .set_write_timeout(Some(common::child_boot_budget()))
            .unwrap();
        frame(
            &mut socket,
            KIND_REQ,
            &serde_json::to_vec(&json!({
            "op": "attach-stream", "id": id}))
            .unwrap(),
        )
        .unwrap();
        let (kind, body) = read_frame(&mut socket).unwrap();
        assert_eq!(kind, KIND_RESP);
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["ok"], true, "attach acknowledgement: {response}");
        let mut drain = socket.try_clone().unwrap();
        drain.set_read_timeout(None).unwrap();
        let received = Arc::new(Mutex::new(Received::default()));
        let log = received.clone();
        let reader = std::thread::spawn(move || {
            while let Ok((kind, body)) = read_frame(&mut drain) {
                let mut received = log.lock().unwrap();
                match kind {
                    KIND_STREAM_OUT => received.output.extend_from_slice(&body),
                    KIND_STREAM_REJECT => received.refusal = Some(body),
                    KIND_STREAM_END => break,
                    _ => {}
                }
            }
            log.lock().unwrap().ended = true;
        });
        Self {
            socket,
            received,
            reader: Some(reader),
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        frame(&mut self.socket, KIND_STREAM_IN, bytes).expect("raw attach input frame");
    }

    pub fn try_send(&mut self, bytes: &[u8]) {
        let _ = frame(&mut self.socket, KIND_STREAM_IN, bytes);
    }

    pub fn output(&self) -> Vec<u8> {
        self.received.lock().unwrap().output.clone()
    }

    pub fn wait_end(&self) {
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                let received = self.received.lock().unwrap();
                received.ended || received.refusal.is_some()
            }),
            "a retired connection must end or refuse instead of remaining stuck behind Full"
        );
        if let Some(refusal) = &self.received.lock().unwrap().refusal {
            assert!(
                [
                    b"exited".as_slice(),
                    b"stale",
                    b"wrong-session",
                    b"history-only"
                ]
                .contains(&refusal.as_slice()),
                "unexpected refusal: {refusal:?}"
            );
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

/// The copied test executable is the stand-in agent. Its private mailbox
/// acknowledges exec, drains stdin and emits queries without typing input.
pub fn agent_process() -> bool {
    let Some(base) = std::env::var_os(MODE) else {
        return false;
    };
    let base = PathBuf::from(base);
    // SAFETY: stdin is this fixture's own slave terminal, never the caller's.
    unsafe {
        let mut term = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &mut term), 0);
        libc::cfmakeraw(&mut term);
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &term), 0);
    }
    let mut capture = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(base.with_extension("input"))
        .unwrap();
    atomic_json(
        &base.with_extension("ready"),
        &json!({
            "exe": std::env::current_exe().unwrap(),
            "pid": std::process::id(),
            "generation": std::env::var("DOT_AGENT_DECK_EDITOR_GEN").ok()
                .map(|g| g.parse::<u64>().unwrap()).unwrap_or(0),
        }),
    );
    loop {
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, no pointer retained; bounds each
        // mailbox observation even when no terminal input is arriving.
        if unsafe { libc::poll(&mut poll, 1, 10) } > 0 {
            let mut bytes = [0; 8192];
            let n = std::io::stdin().read(&mut bytes).unwrap();
            if n == 0 {
                return true;
            }
            capture.write_all(&bytes[..n]).unwrap();
        }
        let path = base.with_extension("command");
        if let Ok(bytes) = fs::read(&path) {
            fs::remove_file(path).unwrap();
            let command: Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(bytes) = command.get("emit") {
                let bytes: Vec<u8> = serde_json::from_value(bytes.clone()).unwrap();
                std::io::stdout().write_all(&bytes).unwrap();
                std::io::stdout().flush().unwrap();
                atomic_json(&base.with_extension("response"), &json!({"ok": true}));
                continue;
            }
            let response = match command.as_str().unwrap() {
                "query" => {
                    std::io::stdout().write_all(QUERY).unwrap();
                    std::io::stdout().flush().unwrap();
                    json!({"ok": true})
                }
                "size" => {
                    let mut size = libc::winsize {
                        ws_row: 0,
                        ws_col: 0,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    // SAFETY: initialized winsize on our own fixture terminal.
                    assert_eq!(unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut size) }, 0);
                    json!({"size": [size.ws_row, size.ws_col]})
                }
                "arm" => helper_publication(),
                "drain" => {
                    // Drain all currently ready input before acknowledging.
                    // The next main-loop poll still catches any later leak.
                    while unsafe { libc::poll(&mut poll, 1, 0) } > 0 {
                        let mut bytes = [0; 8192];
                        let n = std::io::stdin().read(&mut bytes).unwrap();
                        if n == 0 {
                            break;
                        }
                        capture.write_all(&bytes[..n]).unwrap();
                    }
                    json!({"ok": true})
                }
                "exit" => {
                    atomic_json(&base.with_extension("response"), &json!({"exiting": true}));
                    return true;
                }
                other => panic!("unknown fixture command {other}"),
            };
            atomic_json(&base.with_extension("response"), &response);
        }
    }
}

fn helper_publication() -> Value {
    use dot_agent_deck::composer_probe::{arm, instance::InstanceReader};

    let helper = PathBuf::from(std::env::var_os("VISUAL").unwrap());
    let reader = InstanceReader::open(helper.parent().unwrap()).unwrap();
    let generation = std::env::var("DOT_AGENT_DECK_EDITOR_GEN").unwrap();
    let file = reader.arm(&generation).unwrap().unwrap();
    let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
    // SAFETY: a read-only shared mapping of this fixture generation's arm
    // file. The daemon publishes atomic words; the helper reads that ABI.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED);
    // SAFETY: the successful, page-aligned mapping above lives until munmap.
    let publication = unsafe { arm::ArmReader::at(ptr.cast(), len) }
        .unwrap()
        .read_publication();
    // SAFETY: exactly the live mapping above, unmapped once after the read.
    assert_eq!(unsafe { libc::munmap(ptr, len) }, 0);
    let publication = publication.expect("helper must read a consistent quiescent publication");
    json!({"count": publication.count, "seg_seq": publication.seg_seq,
        "native_seq": publication.native_seq})
}
