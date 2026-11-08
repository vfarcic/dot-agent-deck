//! Private fixture: byte-stream capture and acknowledged slave writes.
#![allow(dead_code)] // The two targets exercise different subsets.

use crate::common;
use dot_agent_deck::agent_pty::{AgentPtyRegistry, AttachHandle};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const WAIT: Duration = Duration::from_secs(10);
pub const SENTINEL: &[u8] = b"PREDECESSOR-ONLY-1666\x1b[?2004h\x1b[?2026h-END";

pub fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

pub fn script(root: &Path) -> std::path::PathBuf {
    let path = root.join("slave-writer.py");
    std::fs::write(&path, include_str!("pty_frontier_agent.py")).unwrap();
    path
}

pub fn listener(root: &Path, name: &str) -> UnixListener {
    let listener = UnixListener::bind(root.join(format!("{name}.sock"))).unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

pub struct Peer {
    stream: BufReader<UnixStream>,
    pub ready: Value,
    serial: u64,
    retained: bool,
}

impl Peer {
    pub fn accept(listener: &UnixListener, retained: bool) -> Self {
        let accepted = RefCell::new(None);
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                match listener.accept() {
                    Ok((socket, _)) => {
                        *accepted.borrow_mut() = Some(socket);
                        true
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }),
            "fixture child must acknowledge startup"
        );
        let socket = accepted.into_inner().unwrap();
        socket.set_read_timeout(Some(WAIT)).unwrap();
        socket.set_write_timeout(Some(WAIT)).unwrap();
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        let ready = serde_json::from_str(&line).unwrap();
        Self {
            stream,
            ready,
            serial: 0,
            retained,
        }
    }

    pub fn command(&mut self, action: &str, data: &[u8]) -> Value {
        self.serial += 1;
        let hex: String = data.iter().map(|byte| format!("{byte:02x}")).collect();
        let mut request = serde_json::to_vec(&json!({
            "id": self.serial, "action": action, "hex": hex,
        }))
        .unwrap();
        request.push(b'\n');
        self.stream.get_mut().write_all(&request).unwrap();
        let mut line = String::new();
        self.stream
            .read_line(&mut line)
            .expect("acknowledge actual slave write attempt");
        let reply: Value = serde_json::from_str(&line).expect("fixture JSON acknowledgement");
        assert_eq!(
            reply["id"], self.serial,
            "each command has its own acknowledgement"
        );
        assert_eq!(reply["pid"], self.ready["pid"], "the same writer survived");
        reply
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if self.retained {
            // This pid came from this fixture's connected child. Never use
            // a name-based process kill, even while an assertion unwinds.
            let pid = self.ready["pid"].as_i64().unwrap() as i32;
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

pub fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

pub struct Registry(pub Arc<AgentPtyRegistry>);
impl Drop for Registry {
    fn drop(&mut self) {
        self.0.shutdown_all();
    }
}

pub struct Capture {
    handle: AttachHandle,
    pub bytes: Vec<u8>,
}
impl Capture {
    pub fn new(registry: &AgentPtyRegistry, id: &str) -> Self {
        let handle = registry.subscribe(id).unwrap();
        let bytes = handle.snapshot.clone();
        Self { handle, bytes }
    }
    pub async fn through(&mut self, marker: &[u8]) {
        tokio::time::timeout(WAIT, async {
            while !contains(&self.bytes, marker) {
                let chunk = self
                    .handle
                    .rx
                    .recv()
                    .await
                    .expect("unlagged master capture");
                self.bytes.extend_from_slice(&chunk);
            }
        })
        .await
        .expect("successor-owned output must progress through the capture barrier");
    }
}

/// Binary AttachStream capture, retaining every STREAM_OUT byte, with no
/// vt100 filtering or terminal-sequence interpretation in the oracle.
pub struct WireCapture {
    socket: UnixStream,
    bytes: Arc<Mutex<Vec<u8>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl WireCapture {
    pub fn new(path: &Path, agent: &str) -> Self {
        use dot_agent_deck::daemon_protocol::{
            KIND_REQ, KIND_RESP, KIND_STREAM_END, KIND_STREAM_OUT,
        };
        let mut socket = UnixStream::connect(path).unwrap();
        let payload = serde_json::to_vec(&json!({"op":"attach-stream", "id":agent})).unwrap();
        socket.write_all(&[KIND_REQ]).unwrap();
        socket
            .write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        socket.write_all(&payload).unwrap();
        let mut reader = socket.try_clone().unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let output = bytes.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            loop {
                let mut header = [0; 5];
                if reader.read_exact(&mut header).is_err() {
                    break;
                }
                let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
                assert!(length <= 1024 * 1024, "bounded fixture stream frame");
                let mut chunk = vec![0; length];
                if reader.read_exact(&mut chunk).is_err() {
                    break;
                }
                if header[0] == KIND_RESP {
                    let reply: Value = serde_json::from_slice(&chunk).unwrap();
                    ready_tx.send(reply).unwrap();
                }
                if header[0] == KIND_STREAM_OUT {
                    output.lock().unwrap().extend(chunk);
                }
                if header[0] == KIND_STREAM_END {
                    break;
                }
            }
        });
        let capture = Self {
            socket,
            bytes,
            thread: Some(thread),
        };
        let ready = ready_rx
            .recv_timeout(WAIT)
            .expect("AttachStream subscription readiness");
        assert_eq!(ready["ok"], true, "raw output subscription: {ready}");
        capture
    }
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }
    pub fn through(&self, marker: &[u8]) {
        assert!(
            common::wait_until(WAIT, || contains(&self.bytes.lock().unwrap(), marker)),
            "master stream must contain capture barrier {:?}",
            String::from_utf8_lossy(marker)
        );
    }
    pub fn len(&self) -> usize {
        self.bytes.lock().unwrap().len()
    }
}
impl Drop for WireCapture {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
