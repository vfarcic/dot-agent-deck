//! Local fixtures for the collector's synthetic and real-agent integration tests.

use crate::common;
use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::daemon_protocol::AttachRequest;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

pub fn attribute(value: &Value, key: &str) -> String {
    value["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|attr| attr["key"] == key)
        .and_then(|attr| attr["value"]["stringValue"].as_str())
        .unwrap_or_default()
        .to_string()
}

#[derive(Clone)]
pub struct Request {
    pub path: String,
    pub body: Value,
}

pub struct Receiver {
    addr: SocketAddr,
    pub endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Receiver {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                if let Some(request) = read_request(&stream) {
                    captured.lock().unwrap().push(request);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                }
            }
        });
        Self {
            addr,
            endpoint: format!("http://{addr}"),
            requests,
            stop,
            worker: Some(worker),
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn run(&self, pane: &str) -> Value {
        let find = || {
            self.requests()
                .into_iter()
                .filter(|request| request.path == "/v1/traces")
                .flat_map(|request| {
                    request.body["resourceSpans"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .flat_map(|resource| {
                    resource["scopeSpans"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .flat_map(|scope| scope["spans"].as_array().cloned().unwrap_or_default())
                .find(|span| span["name"] == "agent.run" && attribute(span, "deck.pane.id") == pane)
        };
        assert!(
            common::wait_until(Duration::from_secs(25), || find().is_some()),
            "receiver never received agent.run for pane {pane}"
        );
        find().unwrap()
    }

    pub fn metrics(&self) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == "/v1/metrics")
            .flat_map(|request| {
                request.body["resourceMetrics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter(|resource| {
                attribute(&resource["resource"], "service.name") == "dot-agent-deck-collector"
            })
            .collect()
    }

    pub fn sample(&self, paths: bool) -> Vec<Value> {
        let before = self.requests().len();
        let mut command = collector(self);
        command.arg("--once");
        if paths {
            command.arg("--send-worktree-paths");
        }
        let output = bounded_output(command);
        assert!(
            output.status.success(),
            "collector --once must export resource metrics; status={}; stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = self.requests();
        let metrics: Vec<_> = requests[before..]
            .iter()
            .filter(|request| request.path == "/v1/metrics")
            .collect();
        assert_eq!(
            metrics.len(),
            1,
            "--once must POST exactly once to /v1/metrics"
        );
        metrics[0].body["resourceMetrics"]
            .as_array()
            .expect("OTLP resourceMetrics")
            .clone()
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

fn read_request(stream: &TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut size = 0;
    let mut chunked = false;
    let mut json = false;
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
            "content-length" => size = value.trim().parse().ok()?,
            "transfer-encoding" => chunked = value.trim().eq_ignore_ascii_case("chunked"),
            "content-type" => json = value.contains("json"),
            _ => {}
        }
    }
    let mut bytes = Vec::new();
    if chunked {
        loop {
            line.clear();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                break;
            }
            let start = bytes.len();
            bytes.resize(start + size, 0);
            reader.read_exact(&mut bytes[start..]).ok()?;
            let mut crlf = [0; 2];
            reader.read_exact(&mut crlf).ok()?;
        }
    } else {
        bytes.resize(size, 0);
        reader.read_exact(&mut bytes).ok()?;
    }
    assert!(json, "collector/deck receiver requires application/json");
    Some(Request {
        path,
        body: serde_json::from_slice(&bytes).expect("OTLP JSON body"),
    })
}

pub fn collector(receiver: &Receiver) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck-collector"));
    // Never inherit the real backend or this test pane's run identity.
    command
        .env_clear()
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &receiver.endpoint)
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json")
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "5000");
    command
}

pub fn start_collector(receiver: &Receiver) -> Running {
    let config = collector(receiver);
    // Share the exact same collector artifact and isolated exporter config as
    // --once. This is a separate /proc scraper; the agent never runs it or
    // receives its path. The deck itself always uses the contained TuiDeck launch.
    Running(
        Command::new("/usr/bin/timeout")
            .env_clear()
            .envs(
                config
                    .get_envs()
                    .filter_map(|(key, value)| value.map(|value| (key, value))),
            )
            .arg("180")
            .arg(config.get_program())
            .args(["--interval", "1", "--disk-interval", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

pub struct Running(pub Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn bounded_output(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = std::cell::RefCell::new(Running(command.spawn().expect("start collector/fixture")));
    assert!(
        common::wait_until(Duration::from_secs(30), || child
            .borrow_mut()
            .0
            .try_wait()
            .unwrap()
            .is_some()),
        "collector/fixture must exit within 30 seconds"
    );
    let mut child = child.into_inner();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    Output {
        status: child.0.wait().unwrap(),
        stdout,
        stderr,
    }
}

pub fn close(deck: &TuiDeck, agent: &AgentRecord) {
    let reply = common::attach_request_on(
        deck.attach_socket_path(),
        &AttachRequest::StopAgent {
            id: agent.id.clone(),
        },
    )
    .unwrap();
    assert!(reply.ok, "close pane: {reply:?}");
}

pub fn metric<'a>(resource: &'a Value, name: &str) -> &'a Value {
    resource["scopeMetrics"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|scope| scope["metrics"].as_array().into_iter().flatten())
        .find(|metric| metric["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {resource}"))
}

pub fn run_resource<'a>(metrics: &'a [Value], run_id: &str) -> &'a Value {
    metrics
        .iter()
        .find(|entry| attribute(&entry["resource"], "deck.run.id") == run_id)
        .unwrap_or_else(|| panic!("no resource for deck.run.id={run_id}: {metrics:?}"))
}

pub fn assert_figures(resource: &Value, minimum_cpu: f64) {
    assert_eq!(attribute(&resource["resource"], "deck.attribution"), "run");
    let cpu = metric(resource, "deck.run.cpu.time")["sum"]["dataPoints"][0]["asDouble"]
        .as_f64()
        .unwrap();
    assert!(
        cpu >= minimum_cpu && cpu > 0.0,
        "CPU {cpu} must cover fixture burn {minimum_cpu}"
    );
    let pss = &metric(resource, "deck.run.memory.pss")["gauge"]["dataPoints"];
    let points = pss.as_array().expect("PSS points");
    assert_eq!(points.len(), 3, "PSS must contain anon, file and shmem");
    for kind in ["anon", "file", "shmem"] {
        let point = points
            .iter()
            .find(|point| attribute(point, "deck.memory.kind") == kind)
            .unwrap();
        let bytes: u64 = point["asInt"]
            .as_str()
            .expect("OTLP int64 string")
            .parse()
            .unwrap();
        if kind == "anon" {
            assert!(bytes > 0, "live fixture must have anonymous PSS");
        }
    }
}
