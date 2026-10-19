//! Bounded, test-only startup evidence. No input, geometry changes or retries.
use crate::common;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, KIND_REQ, KIND_RESP, KIND_STREAM_END, KIND_STREAM_OUT, read_frame, write_frame,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct Trace {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    home: PathBuf,
    scratch: PathBuf,
    start: Instant,
    // Keep raw bytes in memory until one redaction pass over the entire stream:
    // a credential split between output frames must not escape redaction.
    data: Mutex<Data>,
}

#[derive(Default)]
struct Data {
    events: Vec<Value>,
    bytes: Vec<u8>,
    event_bytes: usize,
    truncated: bool,
    output_withheld: bool,
}

pub struct StartupTrace {
    pub trace: Trace,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Trace {
    pub fn event(&self, stage: &str, detail: Value) {
        let event = json!({"at":chrono::Utc::now().to_rfc3339(),
            "elapsed_ms":self.inner.start.elapsed().as_millis(), "stage":stage, "detail":detail});
        let size = event.to_string().len();
        let mut data = self.inner.data.lock().unwrap();
        if data.event_bytes + size <= LIMIT {
            data.event_bytes += size;
            data.events.push(event);
        } else {
            data.truncated = true;
        }
    }

    fn frame(&self, kind: u8, body: &[u8]) {
        self.event("pane_frame", json!({"kind":kind,"bytes":body.len()}));
        if kind == KIND_STREAM_OUT {
            let mut data = self.inner.data.lock().unwrap();
            if !data.output_withheld && data.bytes.len() + body.len() <= LIMIT {
                data.bytes.extend_from_slice(body);
            } else if !data.output_withheld {
                // Do not persist a cut credential at the retention boundary.
                data.bytes.clear();
                data.output_withheld = true;
                data.truncated = true;
            }
        } else if kind == KIND_RESP {
            self.event("pane_response", json!(String::from_utf8_lossy(body)));
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut text = common::redact_credentials_for_output(text);
        let mut paths = Vec::new();
        for (path, replacement) in [
            (self.inner.home.clone(), "[scratch-home]"),
            (self.inner.scratch.clone(), "[scratch]"),
            (
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()),
                "[home]",
            ),
        ] {
            if !path.as_os_str().is_empty() {
                text = text.replace(path.to_string_lossy().as_ref(), replacement);
                paths.push(path.to_string_lossy().into_owned());
            }
        }
        // The contained binary lives beside the per-deck workdir/HOME, in
        // their run's outer scratch root.
        if let Some(root) = self.inner.scratch.ancestors().find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("dad-tests-"))
        }) {
            text = text.replace(root.to_string_lossy().as_ref(), "[scratch-run]");
            paths.push(root.to_string_lossy().into_owned());
        }
        // Rendered paths may be split across cursor moves or wrapped rows.
        common::redact_known_credentials_text(&text, &paths)
    }

    fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(values) => values.iter_mut().for_each(|v| self.redact_value(v)),
            Value::Object(fields) => fields.values_mut().for_each(|v| self.redact_value(v)),
            _ => {}
        }
    }

    // Same Snapshot request and 5s read / 10s overall bounds as the existing
    // helper; RESP failures and transport errors are evidence, not empty frames.
    pub fn snapshot(&self, socket: &Path, id: &str) -> Vec<u8> {
        let began = Instant::now();
        let mut out = Vec::new();
        let mut frames = Vec::new();
        let mut retained = 0;
        let result = (|| -> std::io::Result<()> {
            let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let payload = serde_json::to_vec(&AttachRequest::Snapshot { id: id.into() })?;
            stream.write_all(&[KIND_REQ])?;
            stream.write_all(&(payload.len() as u32).to_be_bytes())?;
            stream.write_all(&payload)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let mut header = [0; 5];
                stream.read_exact(&mut header)?;
                let length = u32::from_be_bytes(header[1..].try_into().unwrap()) as usize;
                // Charge every wire frame, including empty/RESP frames, and
                // its metadata against one aggregate snapshot budget.
                if length > LIMIT || retained + header.len() + length > LIMIT {
                    return Err(std::io::Error::other("snapshot frame exceeds trace bound"));
                }
                let mut body = vec![0; length];
                stream.set_read_timeout(Some(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_secs(5))
                        .max(Duration::from_millis(1)),
                ))?;
                stream.read_exact(&mut body)?;
                let frame = json!({"kind":header[0],"bytes":length,
                    "response": if header[0] == KIND_RESP { serde_json::from_slice::<Value>(&body).ok() } else { None }});
                let cost = header.len() + length + frame.to_string().len();
                if retained + cost > LIMIT {
                    return Err(std::io::Error::other(
                        "snapshot metadata exceeds trace bound",
                    ));
                }
                retained += cost;
                frames.push(frame);
                if header[0] == KIND_STREAM_OUT {
                    out.extend_from_slice(&body);
                }
                if header[0] == KIND_STREAM_END {
                    return Ok(());
                }
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "snapshot deadline",
            ))
        })();
        self.event(
            "snapshot",
            json!({"duration_ms":began.elapsed().as_millis(),
            "frames":frames,"bytes":out.len(),"error":result.err().map(|e|e.to_string())}),
        );
        out
    }

    pub fn save(&self, failed: bool) {
        let data = self.inner.data.lock().unwrap();
        let mut document = json!({"events":data.events,"truncated":data.truncated,"output_withheld":data.output_withheld,
            "raw_pane_output":String::from_utf8_lossy(&data.bytes),"failed":failed});
        // Scan real terminal bytes before JSON escapes turn ESC/newline into
        // printable text; the wrapped-credential matcher needs those boundaries.
        self.redact_value(&mut document);
        let _ = std::fs::write(
            self.inner.dir.join("startup-trace.json"),
            document.to_string(),
        );
        drop(data);
        if failed {
            let log_dir = self.inner.home.join(".local/share/opencode/log");
            let output = self.inner.dir.join("opencode-startup-logs");
            let _ = std::fs::create_dir_all(&output);
            let mut copied = Vec::new();
            if let Ok(entries) = std::fs::read_dir(log_dir) {
                for (index, entry) in entries.flatten().take(8).enumerate() {
                    let Ok(meta) = entry.file_type() else {
                        continue;
                    };
                    if !meta.is_file() || entry.path().extension().is_none_or(|ext| ext != "log") {
                        continue;
                    }
                    if let Ok(file) = std::fs::File::open(entry.path()) {
                        let mut bytes = Vec::new();
                        let result = file.take((LIMIT + 1) as u64).read_to_end(&mut bytes);
                        let name = format!("{index}.log");
                        let withheld = bytes.len() > LIMIT;
                        let text = if withheld {
                            "[log withheld: exceeds trace bound]".into()
                        } else {
                            self.redact(&String::from_utf8_lossy(&bytes))
                        };
                        let _ = std::fs::write(output.join(&name), text);
                        copied.push(json!({"file":name,"bytes":bytes.len(),"withheld":withheld,"limit":LIMIT,"error":result.err().map(|e|e.to_string())}));
                    }
                }
            }
            let _ = std::fs::write(
                output.join("manifest.json"),
                json!({"logs":copied,"at":chrono::Utc::now().to_rfc3339()}).to_string(),
            );
        }
    }
}

impl StartupTrace {
    pub fn new(dir: PathBuf, home: PathBuf, scratch: PathBuf, work: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).unwrap();
        let trace = Trace {
            inner: Arc::new(Inner {
                dir,
                home,
                scratch: scratch.clone(),
                start: Instant::now(),
                data: Mutex::new(Data::default()),
            }),
        };
        trace.event(
            "before_start_agent",
            json!({"budget_ms":120000,"sample_ms":100,"limit":LIMIT}),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let sampling = trace.clone();
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut root = None;
            let mut previous = Value::Null;
            let mut seen_lines = std::collections::HashSet::new();
            let mut route_seen = false;
            for _ in 0..1200 {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                if root.is_none()
                    && let Ok(marker) =
                        std::fs::read_to_string(work.join("opencode-launcher.trace"))
                {
                    let fields = marker.split_whitespace().collect::<Vec<_>>();
                    if let Some(pid) = fields.first().and_then(|p| p.parse::<u32>().ok()) {
                        // The launcher PID in this scratch cwd roots the
                        // observed process tree; no host-wide process scan.
                        if std::fs::read_link(format!("/proc/{pid}/cwd")).ok().as_ref()
                            == Some(&work)
                        {
                            sampling.event("launcher_before_exec", json!({"pid":pid,"utc_ns":fields.get(1),"executable":fields.get(2)}));
                            root = process(pid).map(|record| (pid, record["start_ticks"].clone()));
                        }
                    }
                }
                if let Some((pid, ticks)) = &root {
                    let current = process(*pid);
                    if current.as_ref().is_some_and(|p| p["start_ticks"] == *ticks) {
                        let mut records = Vec::new();
                        owned_processes(*pid, &mut records);
                        let processes = json!(records);
                        if processes != previous {
                            sampling.event("owned_processes", processes.clone());
                            previous = processes;
                        }
                    } else if !previous.is_null() {
                        sampling.event(
                            "owned_root_disappeared",
                            json!({"pid":pid,"identity_changed":current.is_some()}),
                        );
                        previous = Value::Null;
                    }
                }
                if !route_seen && scratch.join("opencode-route.json").exists() {
                    sampling.event("first_route_observer_write", json!({}));
                    route_seen = true;
                }
                if let Ok(file) = std::fs::File::open(scratch.join("opencode-inbox-live.log")) {
                    let mut log = String::new();
                    let _ = file.take(LIMIT as u64).read_to_string(&mut log);
                    for line in log.lines().filter(|line| {
                        line.contains("receipt receiver registered")
                            || line.contains("inbox: armed")
                    }) {
                        if seen_lines.insert(line.to_owned()) {
                            sampling.event("receiver", json!(line));
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            trace,
            stop,
            threads: vec![thread],
        }
    }

    pub fn attach(&mut self, socket: PathBuf, id: String) {
        let trace = self.trace.clone();
        let stop = self.stop.clone();
        self.threads.push(std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let result = runtime.block_on(async {
                let mut stream = tokio::time::timeout(Duration::from_secs(5), async {
                    let mut stream = tokio::net::UnixStream::connect(socket).await?;
                    let request = AttachRequest::AttachStream { id, rows: None, cols: None, geometry_updates: false, client_id: None };
                    write_frame(&mut stream, KIND_REQ, &serde_json::to_vec(&request)?).await?;
                    Ok::<_, std::io::Error>(stream)
                }).await.map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "passive attach deadline"))??;
                // Keep a pending frame read alive across ticks. Cancelling a
                // partial read on each tick could discard part of the header.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
                loop {
                    let frame = read_frame(&mut stream);
                    tokio::pin!(frame);
                    let received = loop {
                        tokio::select! {
                            result = &mut frame => break result?,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                                if stop.load(Ordering::Relaxed) || tokio::time::Instant::now() >= deadline { return Ok::<_, std::io::Error>(()); }
                            }
                        }
                    };
                    match received {
                        Some((kind, bytes)) => { trace.frame(kind, &bytes); if kind == KIND_STREAM_END { return Ok(()); } }
                        None => return Err(std::io::Error::other("passive stream EOF")),
                    }
                }
            });
            trace.event("passive_stream_end", json!({"error":result.err().map(|e|e.to_string())}));
        }));
    }
}

impl Drop for StartupTrace {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.trace.save(std::thread::panicking())
        }));
    }
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<Value> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    Some(
        json!({"pid":pid,"state":fields.first(),"ppid":fields.get(1),"start_ticks":fields.get(19),
        "comm":std::fs::read_to_string(format!("/proc/{pid}/comm")).ok(),
        "exe":std::fs::read_link(format!("/proc/{pid}/exe")).ok(),
        "wchan":std::fs::read_to_string(format!("/proc/{pid}/wchan")).ok()}),
    )
}

#[cfg(not(target_os = "linux"))]
fn process(_pid: u32) -> Option<Value> {
    None
}

fn owned_processes(pid: u32, records: &mut Vec<Value>) {
    if records.len() >= 32 {
        return;
    }
    let Some(record) = process(pid) else {
        return;
    };
    records.push(record);
    if let Ok(children) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for child in children
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            owned_processes(child, records);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_trace_retains_redacted_frames_and_logs_during_unwinding() {
        let scratch = common::harness_tempdir().unwrap();
        let home = scratch.path().join("home");
        let work = scratch.path().join("project");
        let dir = scratch.path().join("diagnostics");
        let logs = home.join(".local/share/opencode/log");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let credential = "OPENCODE-TRACE-FAKE-SECRET-7e9d";
        common::register_diagnostic_redactions([credential.to_owned()]);
        std::fs::write(
            logs.join("startup.log"),
            format!(
                "initializing {credential} {} {}",
                home.display(),
                work.display()
            ),
        )
        .unwrap();
        let result = std::panic::catch_unwind(|| {
            let trace = StartupTrace::new(dir.clone(), home, scratch.path().into(), work);
            trace.trace.frame(KIND_STREAM_OUT, b"OPENCODE-TRACE-FAKE-");
            trace
                .trace
                .frame(KIND_STREAM_OUT, b"SECRET-7e9d\x1b[0mAsk anything");
            trace
                .trace
                .frame(KIND_STREAM_OUT, b"OPENCODE-TRACE-FAKE-\x1b[2;1HSECRET-7e9d");
            panic!("synthetic startup failure");
        });
        assert!(result.is_err());
        let saved = std::fs::read_to_string(dir.join("startup-trace.json")).unwrap();
        assert!(!saved.contains(credential));
        assert!(!saved.contains("OPENCODE-TRACE-FAKE-"));
        assert!(!saved.contains("SECRET-7e9d"));
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved["failed"], true);
        assert!(
            saved["raw_pane_output"]
                .as_str()
                .unwrap()
                .contains("Ask anything")
        );
        let log = std::fs::read_to_string(dir.join("opencode-startup-logs/0.log")).unwrap();
        assert!(log.contains("initializing"));
        assert!(!log.contains(credential));
        assert!(!log.contains(scratch.path().to_str().unwrap()));
    }

    #[test]
    fn startup_trace_records_snapshot_refusal_and_transport_error() {
        let scratch = common::harness_tempdir().unwrap();
        let socket = scratch.path().join("snapshot.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 5];
            stream.read_exact(&mut header).unwrap();
            let mut body = vec![0; u32::from_be_bytes(header[1..].try_into().unwrap()) as usize];
            stream.read_exact(&mut body).unwrap();
            for (kind, body) in [
                (
                    KIND_RESP,
                    br#"{"ok":false,"error":"no such agent"}"#.as_slice(),
                ),
                (KIND_STREAM_END, b"".as_slice()),
            ] {
                stream.write_all(&[kind]).unwrap();
                stream
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let startup = StartupTrace::new(
            scratch.path().join("diagnostics"),
            scratch.path().join("home"),
            scratch.path().into(),
            scratch.path().join("work"),
        );
        assert!(startup.trace.snapshot(&socket, "missing").is_empty());
        assert!(
            startup
                .trace
                .snapshot(&scratch.path().join("absent.sock"), "missing")
                .is_empty()
        );
        server.join().unwrap();
        let data = startup.trace.inner.data.lock().unwrap();
        let snapshots = data
            .events
            .iter()
            .filter(|event| event["stage"] == "snapshot")
            .collect::<Vec<_>>();
        assert_eq!(snapshots[0]["detail"]["frames"][0]["response"]["ok"], false);
        assert_eq!(
            snapshots[0]["detail"]["frames"][0]["response"]["error"],
            "no such agent"
        );
        assert!(snapshots[1]["detail"]["error"].is_string());
        drop(data);

        // A stream of responses (or empty output frames) must leave bounded
        // evidence of refusal, rather than losing the entire oversized event.
        for kind in [KIND_RESP, KIND_STREAM_OUT] {
            let startup = StartupTrace::new(
                scratch.path().join(format!("flood-diagnostics-{kind}")),
                scratch.path().join("home"),
                scratch.path().into(),
                scratch.path().join("work"),
            );
            let listener = std::os::unix::net::UnixListener::bind(
                scratch.path().join(format!("flood-{kind}.sock")),
            )
            .expect("bind snapshot flood fixture");
            let socket = listener.local_addr().expect("snapshot flood address");
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept snapshot flood");
                let mut header = [0; 5];
                stream
                    .read_exact(&mut header)
                    .expect("read snapshot request header");
                let mut request = vec![
                    0;
                    u32::from_be_bytes(header[1..].try_into().expect("four length bytes"))
                        as usize
                ];
                stream
                    .read_exact(&mut request)
                    .expect("read snapshot request");
                let body = if kind == KIND_RESP {
                    br#"{"ok":true}"#.as_slice()
                } else {
                    b"".as_slice()
                };
                let mut frames = Vec::new();
                for _ in 0..LIMIT / 16 {
                    frames.push(kind);
                    frames.extend_from_slice(&(body.len() as u32).to_be_bytes());
                    frames.extend_from_slice(body);
                }
                frames.push(KIND_STREAM_END);
                frames.extend_from_slice(&0_u32.to_be_bytes());
                // The bounded reader may close while this flood is in flight.
                let _ = stream.write_all(&frames);
            });
            assert!(
                startup
                    .trace
                    .snapshot(socket.as_pathname().expect("pathname socket"), "flood")
                    .is_empty()
            );
            server.join().expect("snapshot flood server completes");
            let data = startup.trace.inner.data.lock().expect("startup trace data");
            let event = data
                .events
                .iter()
                .rev()
                .find(|event| event["stage"] == "snapshot")
                .expect("snapshot flood must retain bounded refusal evidence");
            assert!(
                event["detail"]["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("bound")),
                "snapshot flood must report its aggregate bound: {}",
                event["detail"]["error"]
            );
            assert!(
                event["detail"]["frames"].to_string().len() <= LIMIT,
                "snapshot frame metadata must stay within the aggregate trace budget"
            );
        }
    }
}
