//! A transparent, test-owned attach socket: drop transports without restarting
//! the daemon or its PTYs. No production disconnect seam is needed.
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct AttachProxy {
    _dir: tempfile::TempDir,
    pub socket: PathBuf,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    attaches: Arc<std::sync::atomic::AtomicUsize>,
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
        let worker = {
            let connections = Arc::clone(&connections);
            let stop = Arc::clone(&stop);
            let upstream = upstream.to_path_buf();
            let attaches = Arc::clone(&attaches);
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
                                    let attach = header[0]
                                        == dot_agent_deck::daemon_protocol::KIND_REQ
                                        && serde_json::from_slice::<serde_json::Value>(&payload)
                                            .is_ok_and(|request| request["op"] == "attach-stream");
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
                                let _ = std::io::copy(&mut upstream_read, &mut downstream_write);
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
}

impl Drop for AttachProxy {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
        self.disconnect();
    }
}
