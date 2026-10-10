//! A local stand-in for GitHub's release API and download host (issue #1635),
//! shared by the self-upgrade scenarios and the docs screenshot of the TUI's
//! upgrade dialog.
//!
//! The binary under test is pointed at it through `e2e`-only seams
//! ([`FakeReleases::env`]): where releases are looked up and downloaded, and
//! the version and path the running copy reports.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use dot_agent_deck::self_upgrade::Platform;

/// A stand-in for a release binary: a script that answers exactly one
/// invocation, `--version`, as dot-agent-deck `version`, and fails on any
/// other. The core checks a download only by its checksum and that answer, so
/// the script exercises the whole download → check → replace path without
/// moving a few hundred megabytes of debug binary, and a core that ran the
/// download any other way would see it fail instead of an answer.
pub(crate) fn release_script(version: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nif [ \"$#\" -eq 1 ] && [ \"$1\" = \"--version\" ]; then\n  echo 'dot-agent-deck {version}'\n  exit 0\nfi\necho 'unexpected invocation' >&2\nexit 64\n"
    )
    .into_bytes()
}

/// `bytes`' SHA-256, as `checksums.txt` lists it.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The platform this test runs on, when releases ship assets for it.
pub(crate) fn platform() -> Option<Platform> {
    Platform::current()
}

/// This platform's CLI release asset, the one file besides `checksums.txt`
/// the server offers for download.
pub(crate) fn cli_asset() -> &'static str {
    Platform::current()
        .expect("a platform with release assets")
        .cli_asset()
}

pub(crate) struct FakeReleases {
    port: u16,
    latest: Arc<Mutex<String>>,
}

impl FakeReleases {
    /// Serve `latest` as the newest stable release, its CLI asset as `asset`,
    /// and `manifest` as its `checksums.txt`.
    pub(crate) fn start(latest: &str, asset: Vec<u8>, manifest: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake release server");
        let port = listener.local_addr().unwrap().port();
        let latest = Arc::new(Mutex::new(latest.to_string()));
        let shared = latest.clone();
        let asset = Arc::new(asset);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let latest = shared.lock().unwrap().clone();
                let asset = asset.clone();
                let manifest = manifest.clone();
                std::thread::spawn(move || serve(stream, &latest, &asset, &manifest));
            }
        });
        Self { port, latest }
    }

    pub(crate) fn set_latest(&self, version: &str) {
        *self.latest.lock().unwrap() = version.to_string();
    }

    pub(crate) fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The seams that point the binary under test at this server, as a copy
    /// running `running_version` from `exe`.
    pub(crate) fn env(&self, exe: &Path, running_version: &str) -> Vec<(String, String)> {
        vec![
            (
                "DOT_AGENT_DECK_TEST_RELEASES_API_URL".into(),
                format!("{}/api/latest", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL".into(),
                format!("{}/api/list", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RELEASE_DOWNLOAD_BASE".into(),
                format!("{}/download", self.base()),
            ),
            (
                "DOT_AGENT_DECK_TEST_RUNNING_VERSION".into(),
                running_version.into(),
            ),
            (
                "DOT_AGENT_DECK_TEST_RUNNING_EXE".into(),
                exe.to_str().expect("UTF-8 path").into(),
            ),
        ]
    }
}

fn serve(mut stream: TcpStream, latest: &str, asset: &[u8], manifest: &str) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let release = format!(r#"{{"tag_name":"v{latest}","draft":false,"prerelease":false}}"#);
    let download = format!("/download/v{latest}/");
    let (status, body): (&str, Vec<u8>) = if path == "/api/latest" {
        ("200 OK", release.into_bytes())
    } else if path.starts_with("/api/list") {
        ("200 OK", format!("[{release}]").into_bytes())
    } else if let Some(name) = path.strip_prefix(&download) {
        if name == "checksums.txt" {
            ("200 OK", manifest.as_bytes().to_vec())
        } else if name == cli_asset() {
            ("200 OK", asset.to_vec())
        } else {
            ("404 Not Found", Vec::new())
        }
    } else {
        ("404 Not Found", Vec::new())
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
}
