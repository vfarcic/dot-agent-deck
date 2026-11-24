//! A test-owned client that answers a pane's cursor position queries as the
//! desktop app's terminal does (xterm.js answers `CSI 6 n` and its `onData`
//! forwards the reply to the pane). Shared by the lane-2 real-Codex harness
//! (`codex_installed.rs`) and the lane-1 CPR stand-in tests
//! (`tests/e2e_inbox_redraw_wait.rs`, `inbox/probe/105`). It stands in for a
//! desktop; it is not the desktop's own answer path.
#![allow(dead_code)] // Each test file uses its own subset.

use std::time::Duration;

/// A client attached to a pane that answers every cursor position query
/// (`CSI 6 n`, `CSI ? 6 n`) in the pane's output with the cursor where its
/// own terminal model puts it, as the desktop app's terminal does. The TUI
/// answers none, so without this a TUI-attached deck is, for these queries,
/// headless. **Test-owned, and not C1 coverage**: it stands in for a
/// desktop; it is not the desktop's own answer path.
pub struct CprAnswerer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    answered: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    size: std::sync::Arc<std::sync::Mutex<(u16, u16)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CprAnswerer {
    /// Attach to `agent` on `socket` with a terminal model of `rows` ×
    /// `cols`, and answer from now on.
    pub fn attach(socket: &std::path::Path, agent: &str, rows: u16, cols: u16) -> Self {
        use dot_agent_deck::daemon_protocol::{
            AttachRequest, KIND_REQ, KIND_STREAM_IN, KIND_STREAM_OUT,
        };
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let mut stream =
            std::os::unix::net::UnixStream::connect(socket).expect("connect to the attach socket");
        let request = AttachRequest::AttachStream {
            id: agent.to_string(),
            rows: None,
            cols: None,
            geometry_updates: false,
            client_id: None,
        };
        let payload = serde_json::to_vec(&request).unwrap();
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        stream.write_all(&header).unwrap();
        stream.write_all(&payload).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicUsize::new(0));
        let size = Arc::new(Mutex::new((rows, cols)));
        let (stop_t, answered_t, size_t) = (stop.clone(), answered.clone(), size.clone());
        let thread = std::thread::spawn(move || {
            let mut writer = stream.try_clone().unwrap();
            let mut model = dot_agent_deck::vt_grid::parser(rows, cols, 0);
            let mut model_size = (rows, cols);
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 16384];
            let mut tail: Vec<u8> = Vec::new();
            while !stop_t.load(Ordering::Relaxed) {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
                while buf.len() >= 5 {
                    let len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
                    if buf.len() < 5 + len {
                        break;
                    }
                    let kind = buf[0];
                    let body: Vec<u8> = buf[5..5 + len].to_vec();
                    buf.drain(..5 + len);
                    if kind != KIND_STREAM_OUT {
                        continue;
                    }
                    let wanted = *size_t.lock().unwrap();
                    if wanted != model_size {
                        dot_agent_deck::vt_grid::set_size(model.screen_mut(), wanted.0, wanted.1);
                        model_size = wanted;
                    }
                    // Feed up to each query, answer it from the model's cursor.
                    // A query split across frames is found with the previous
                    // frame's last bytes (`tail`).
                    let mut joined = std::mem::take(&mut tail);
                    let fed_from = joined.len();
                    joined.extend_from_slice(&body);
                    let mut fed = fed_from;
                    let mut at = 0;
                    while let Some(found) = find_cpr(&joined[at..]) {
                        let end = at + found.1;
                        if end > fed {
                            model.process(&joined[fed..end]);
                            fed = end;
                        }
                        let (row, col) = model.screen().cursor_position();
                        let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                        let mut header = [0u8; 5];
                        header[0] = KIND_STREAM_IN;
                        header[1..].copy_from_slice(&(reply.len() as u32).to_be_bytes());
                        if writer.write_all(&header).is_err()
                            || writer.write_all(reply.as_bytes()).is_err()
                        {
                            return;
                        }
                        answered_t.fetch_add(1, Ordering::Relaxed);
                        at = end;
                    }
                    if fed < joined.len() {
                        model.process(&joined[fed..]);
                    }
                    let keep = joined.len().saturating_sub(5).max(at);
                    tail = joined[keep..].to_vec();
                    // Bytes already fed must not be fed again: keep them only
                    // to find a split query, then skip them next time.
                    tail = if find_cpr_prefix(&tail) {
                        tail
                    } else {
                        Vec::new()
                    };
                }
            }
        });
        Self {
            stop,
            answered,
            size,
            thread: Some(thread),
        }
    }

    /// How many queries it has answered.
    pub fn answered(&self) -> usize {
        self.answered.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The pane's terminal is now `rows` × `cols`.
    pub fn resize(&self, rows: u16, cols: u16) {
        *self.size.lock().unwrap() = (rows, cols);
    }
}

impl Drop for CprAnswerer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The first cursor position query in `bytes`: its start and end.
fn find_cpr(bytes: &[u8]) -> Option<(usize, usize)> {
    for (query, len) in [(&b"\x1b[6n"[..], 4), (&b"\x1b[?6n"[..], 5)] {
        if let Some(i) = bytes.windows(len).position(|w| w == query) {
            return Some((i, i + len));
        }
    }
    None
}

/// Whether `tail` ends with the start of a query, which the next frame may
/// complete.
fn find_cpr_prefix(tail: &[u8]) -> bool {
    tail.iter().rposition(|b| *b == 0x1b).is_some_and(|i| {
        let rest = &tail[i..];
        (b"\x1b[6n".starts_with(rest) && rest.len() < 4)
            || (b"\x1b[?6n".starts_with(rest) && rest.len() < 5)
    })
}
