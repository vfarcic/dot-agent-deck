//! A transparent, test-owned attach socket: drop transports without restarting
//! the daemon or its PTYs. No production disconnect seam is needed.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub struct AttachProxy {
    _dir: tempfile::TempDir,
    pub socket: PathBuf,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    attaches: Arc<std::sync::atomic::AtomicUsize>,
    subscriptions: Arc<std::sync::atomic::AtomicUsize>,
    events_held: Arc<(Mutex<bool>, Condvar)>,
    terminal_eof_held: Arc<(Mutex<bool>, Condvar)>,
}

impl AttachProxy {
    pub fn new(upstream: &Path) -> Self {
        let dir = crate::common::harness_tempdir().unwrap();
        let socket = dir.path().join("attach.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let connections = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let attaches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let subscriptions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events_held = Arc::new((Mutex::new(false), Condvar::new()));
        let terminal_eof_held = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = {
            let connections = Arc::clone(&connections);
            let stop = Arc::clone(&stop);
            let upstream = upstream.to_path_buf();
            let attaches = Arc::clone(&attaches);
            let subscriptions = Arc::clone(&subscriptions);
            let events_held = Arc::clone(&events_held);
            let terminal_eof_held = Arc::clone(&terminal_eof_held);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut client, _)) => {
                            let Ok(mut daemon) = UnixStream::connect(&upstream) else {
                                continue;
                            };
                            let mut upstream_read = daemon.try_clone().unwrap();
                            let mut downstream_write = client.try_clone().unwrap();
                            connections
                                .lock()
                                .unwrap()
                                .push(client.try_clone().unwrap());
                            connections
                                .lock()
                                .unwrap()
                                .push(daemon.try_clone().unwrap());
                            let attaches = Arc::clone(&attaches);
                            let events_held = Arc::clone(&events_held);
                            let terminal = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let terminal_writer = Arc::clone(&terminal);
                            let event_reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let event_writer = Arc::clone(&event_reply);
                            let subscriptions = Arc::clone(&subscriptions);
                            let terminal_eof_held = Arc::clone(&terminal_eof_held);
                            std::thread::spawn(move || {
                                let mut header = [0; 5];
                                while client.read_exact(&mut header).is_ok() {
                                    let len = u32::from_be_bytes(header[1..].try_into().unwrap())
                                        as usize;
                                    assert!(len <= 1024 * 1024, "bounded fixture frame");
                                    let mut payload = vec![0; len];
                                    if client.read_exact(&mut payload).is_err() {
                                        break;
                                    }
                                    let request = (header[0]
                                        == dot_agent_deck::daemon_protocol::KIND_REQ)
                                        .then(|| {
                                            serde_json::from_slice::<serde_json::Value>(&payload)
                                                .ok()
                                        })
                                        .flatten();
                                    let attach = request
                                        .as_ref()
                                        .is_some_and(|r| r["op"] == "attach-stream");
                                    if attach {
                                        terminal_writer
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    if request.as_ref().is_some_and(|r| {
                                        matches!(
                                            r["op"].as_str(),
                                            Some(
                                                "subscribe-events"
                                                    | "subscribe-events-with-snapshot"
                                            )
                                        )
                                    }) {
                                        let (held, ready) = &*events_held;
                                        drop(
                                            ready
                                                .wait_while(held.lock().unwrap(), |held| *held)
                                                .unwrap(),
                                        );
                                        event_writer
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    if daemon
                                        .write_all(&header)
                                        .and_then(|_| daemon.write_all(&payload))
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if attach {
                                        attaches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                                let _ = daemon.shutdown(std::net::Shutdown::Both);
                            });
                            std::thread::spawn(move || {
                                let mut header = [0; 5];
                                while upstream_read.read_exact(&mut header).is_ok() {
                                    // Inspect before forwarding Hello: its caller
                                    // cannot yet have submitted the subscription.
                                    let subscribed = header[0]
                                        == dot_agent_deck::daemon_protocol::KIND_RESP
                                        && event_reply
                                            .swap(false, std::sync::atomic::Ordering::SeqCst);
                                    let len = u32::from_be_bytes(header[1..].try_into().unwrap())
                                        as usize;
                                    assert!(len <= 1024 * 1024, "bounded fixture response");
                                    let mut payload = vec![0; len];
                                    if upstream_read.read_exact(&mut payload).is_err() {
                                        break;
                                    }
                                    if header[0] == dot_agent_deck::daemon_protocol::KIND_STREAM_END
                                        && terminal.load(std::sync::atomic::Ordering::SeqCst)
                                    {
                                        let (held, ready) = &*terminal_eof_held;
                                        drop(
                                            ready
                                                .wait_while(held.lock().unwrap(), |held| *held)
                                                .unwrap(),
                                        );
                                    }
                                    if downstream_write
                                        .write_all(&header)
                                        .and_then(|_| downstream_write.write_all(&payload))
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if subscribed {
                                        subscriptions
                                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                                if terminal.load(std::sync::atomic::Ordering::SeqCst) {
                                    let (held, ready) = &*terminal_eof_held;
                                    drop(
                                        ready
                                            .wait_while(held.lock().unwrap(), |held| *held)
                                            .unwrap(),
                                    );
                                }
                                let _ = downstream_write.shutdown(std::net::Shutdown::Both);
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(error) => panic!("test attach proxy: {error}"),
                    }
                }
            })
        };
        Self {
            _dir: dir,
            socket,
            connections,
            stop,
            worker: Some(worker),
            attaches,
            subscriptions,
            events_held,
            terminal_eof_held,
        }
    }

    pub fn disconnect(&self) {
        for connection in self.connections.lock().unwrap().drain(..) {
            let _ = connection.shutdown(std::net::Shutdown::Both);
        }
    }

    pub fn attach_count(&self) -> usize {
        self.attaches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Subscription replies already forwarded from the fixture daemon.
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Hold new event subscriptions, leaving action requests free to reach
    /// their own Hello gate. This orders stale keyboard actions before the
    /// subscriber can learn a replacement's process, without timing sleeps.
    pub fn hold_events(&self, hold: bool) {
        *self.events_held.0.lock().unwrap() = hold;
        self.events_held.1.notify_all();
    }

    /// Keep the original terminal transport open after its daemon exits so
    /// a keyboard action can precede automatic reattachment and local drop.
    pub fn hold_terminal_eof(&self, hold: bool) {
        *self.terminal_eof_held.0.lock().unwrap() = hold;
        self.terminal_eof_held.1.notify_all();
    }
}

impl Drop for AttachProxy {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.hold_events(false);
        self.hold_terminal_eof(false);
        self.worker.take().unwrap().join().unwrap();
        self.disconnect();
    }
}
