//! Local JSON OTLP receiver shared by operation-completion integration tests.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

pub const EXPORT_WAIT: Duration = Duration::from_secs(25);

pub struct Receiver {
    addr: SocketAddr,
    pub endpoint: String,
    spans: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Receiver {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let spans = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&spans);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                // Agent SDKs can cancel an empty connection at shutdown.
                let Some((path, body, protobuf)) = read_request(&stream) else {
                    continue;
                };
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
                let _ = stream.write_all(response);
            }
        });
        Self {
            addr,
            endpoint: format!("http://{addr}"),
            spans,
            stop,
            worker: Some(worker),
        }
    }

    pub fn spans(&self) -> Vec<Value> {
        self.spans.lock().unwrap().clone()
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

fn read_request(stream: &TcpStream) -> Option<(String, Vec<u8>, bool)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut length = 0;
    let mut chunked = false;
    let mut protobuf = false;
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.trim_end().split_once(':')?;
        match key.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "transfer-encoding" => chunked = value.trim().eq_ignore_ascii_case("chunked"),
            "content-type" => protobuf = value.contains("protobuf"),
            _ => {}
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line).ok()?;
            let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).ok()?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).ok()?;
        }
    } else {
        body.resize(length, 0);
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
