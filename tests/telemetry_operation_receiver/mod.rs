//! Local JSON OTLP receiver shared by operation-completion integration tests.

use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(test)]
mod bounds;

pub const EXPORT_WAIT: Duration = Duration::from_secs(25);
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const READ_POLL: Duration = Duration::from_millis(100);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Receiver {
    addr: SocketAddr,
    pub endpoint: String,
    spans: Arc<Mutex<Vec<Value>>>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Receiver {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&spans);
        let captured_bodies = Arc::clone(&bodies);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                // Agent SDKs can cancel an empty connection at shutdown.
                let Some((path, body, protobuf)) = read_request(&stream, &stopped) else {
                    continue;
                };
                captured_bodies.lock().unwrap().push(body.clone());
                if path == "/v1/traces"
                    && let Ok(body) = serde_json::from_slice::<Value>(&body)
                {
                    for resource in body["resourceSpans"].as_array().into_iter().flatten() {
                        for scope in resource["scopeSpans"].as_array().into_iter().flatten() {
                            captured
                                .lock()
                                .unwrap()
                                .extend(scope["spans"].as_array().into_iter().flatten().cloned());
                        }
                    }
                }
                let response: &[u8] = if protobuf {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                };
                let _ = stream.set_write_timeout(Some(READ_POLL));
                let _ = stream.write_all(response);
            }
        });
        Self {
            addr,
            endpoint: format!("http://{addr}"),
            spans,
            bodies,
            stop,
            worker: Some(worker),
        }
    }

    pub fn spans(&self) -> Vec<Value> {
        self.spans.lock().unwrap().clone()
    }

    #[allow(dead_code)] // Only privacy contract targets need raw payloads.
    pub fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().clone()
    }

    pub fn operations(&self, pane: &str) -> Vec<Value> {
        self.spans()
            .into_iter()
            .filter(|span| {
                span["name"] == "agent.operation" && attribute(span, "deck.pane.id") == Some(pane)
            })
            .collect()
    }

    pub fn run(&self, pane: &str) -> Option<Value> {
        self.spans().into_iter().find(|span| {
            span["name"] == "agent.run" && attribute(span, "deck.pane.id") == Some(pane)
        })
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct RequestReader<'a> {
    stream: &'a TcpStream,
    stop: &'a AtomicBool,
    deadline: Instant,
}

impl Read for RequestReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.stream.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}

fn read_line(reader: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    let length = reader
        .take((MAX_LINE_BYTES + 1) as u64)
        .read_line(&mut line)
        .ok()?;
    (length <= MAX_LINE_BYTES && line.ends_with("\r\n")).then_some(line)
}

fn read_request(stream: &TcpStream, stop: &AtomicBool) -> Option<(String, Vec<u8>, bool)> {
    stream.set_read_timeout(Some(READ_POLL)).ok()?;
    let mut reader = BufReader::new(RequestReader {
        stream,
        stop,
        deadline: Instant::now() + REQUEST_TIMEOUT,
    });
    let line = read_line(&mut reader)?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut header_bytes = line.len();
    let mut length = None;
    let mut chunked = false;
    let mut protobuf = false;
    loop {
        let line = read_line(&mut reader)?;
        header_bytes = header_bytes.checked_add(line.len())?;
        if header_bytes > MAX_HEADER_BYTES {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.trim_end().split_once(':')?;
        match key.to_ascii_lowercase().as_str() {
            "content-length" => {
                let parsed = value.trim().parse::<usize>().ok()?;
                if parsed > MAX_BODY_BYTES || length.replace(parsed).is_some() {
                    return None;
                }
            }
            "transfer-encoding" => {
                if !value.trim().eq_ignore_ascii_case("chunked") {
                    return None;
                }
                chunked = true;
            }
            "content-type" => protobuf = value.contains("protobuf"),
            _ => {}
        }
    }
    if chunked && length.is_some() {
        return None;
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let line = read_line(&mut reader)?;
            let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                if read_line(&mut reader)? != "\r\n" {
                    return None;
                }
                break;
            }
            let start = body.len();
            let end = start.checked_add(size)?;
            if end > MAX_BODY_BYTES {
                return None;
            }
            body.resize(end, 0);
            reader.read_exact(&mut body[start..]).ok()?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).ok()?;
            if crlf != *b"\r\n" {
                return None;
            }
        }
    } else {
        body.resize(length.unwrap_or(0), 0);
        reader.read_exact(&mut body).ok()?;
    }
    Some((path, body, protobuf))
}

pub fn attribute<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)?["value"]["stringValue"]
        .as_str()
}

pub fn nanos(span: &Value, key: &str) -> u64 {
    span[key].as_str().unwrap().parse().unwrap()
}
