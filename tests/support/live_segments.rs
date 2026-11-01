//! A native synthetic agent owns the real installed helper; the reviewed F2
//! Python composer supplies its screen, input map and loss mechanisms. The
//! composer asks its native parent to invoke VISUAL through an atomic mailbox,
//! so helper ancestry is the genuine agent -> helper path, not a Bash tool.
//! No production state or observation is injected by that mailbox.
#![allow(dead_code)] // Individual scenarios use different fixture controls.

use crate::{common, inbox};
use dot_agent_deck::composer_probe::arm::{ArmReader, Publication};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

pub const WAIT: Duration = Duration::from_secs(10);
const MODE: &str = "DAD_LIVE_SEGMENTS_BASE";
const PANE: &str = "live-segments";

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

pub fn atomic(path: &Path, value: &Value) {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temp.write_all(&serde_json::to_vec(value).unwrap()).unwrap();
    temp.persist(path).unwrap();
}

pub fn read_json(path: &Path) -> Value {
    assert!(
        common::wait_until(common::child_boot_budget(), || {
            fs::read(path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some()
        }),
        "fixture acknowledgement missing: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn checked_replace(source: &str, anchor: &str, replacement: &str) -> String {
    assert_eq!(
        source.matches(anchor).count(),
        1,
        "live-segments fixture patch must match exactly once: {anchor:?}"
    );
    source.replacen(anchor, replacement, 1)
}

pub struct Fixture {
    pub daemon: common::DaemonProc,
    pub base: PathBuf,
    pub agent: String,
    pub generation: u64,
    pub kind: &'static str,
    instance: PathBuf,
    exe: PathBuf,
}

impl Fixture {
    pub fn start(kind: &'static str) -> Self {
        Self::with_env(kind, &[])
    }

    pub fn with_env(kind: &'static str, extra: &[(&str, &str)]) -> Self {
        let mut env = vec![
            (
                "DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE",
                "claude@2.1.310,codex@0.164.0",
            ),
            ("DOT_AGENT_DECK_DOORBELL_TICK_MS", "25"),
        ];
        env.extend_from_slice(extra);
        let daemon = common::spawn_daemon_serve_with_env(None, "0", &env);
        let native = daemon.home.join("native");
        fs::create_dir(&native).unwrap();
        let exe = native.join(kind);
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let composer = checked_replace(
            include_str!("composer_external_edit.py"),
            "# M31: keys unread by an editor during suspension disappear.",
            "# Optional adversarial post-enable flush witness, local to S-20.\n                if self.path('.gate-flush').exists():\n                    os.write(1, b'\\x1b[?2004h')\n                    self.path('.flush-arrived').touch()\n                    while not self.path('.flush-go').exists(): time.sleep(.005)\n                # M31: keys unread by an editor during suspension disappear.",
        );
        let composer = checked_replace(
            &composer,
            "    def tokens(self, chunk):\n",
            "    def tokens(self, chunk):\n        # A configured whole terminal report is inert to this stand-in.\n        # Candidate histories deliberately retain F2's normal Ctrl+G map.\n        report = self.path('.reported-osc')\n        if report.exists(): chunk = chunk.replace(report.read_bytes(), b'')\n",
        );
        let composer = checked_replace(
            &composer,
            "            path.unlink(missing_ok=True)\n            os.write(1, b'\\x1b[?2004h')",
            "            path.unlink(missing_ok=True)\n            os.write(1, b'\\x1b[?2004h')\n            self.return_count = getattr(self, 'return_count', 0) + 1\n            self.path(f'.composer-return-{self.return_count}').touch()",
        );
        fs::write(native.join("composer_external_edit.py"), composer).unwrap();
        fs::write(
            native.join("composer.py"),
            if kind == "codex" {
                include_str!("codex_composer.py")
            } else {
                include_str!("claude_composer.py")
            },
        )
        .unwrap();
        let editor = checked_replace(
            include_str!("composer_test_editor.py"),
            "mode, base, *args = sys.argv[1:]",
            "mode, base, *args = sys.argv[1:]\nmode_file = Path(base + '.editor-mode')\nif mode_file.exists(): mode = mode_file.read_text().strip()",
        );
        let editor = checked_replace(
            &editor,
            "path.write_bytes(b'EDITED",
            "if mode == 'survivor':\n    survivor = os.fork()\n    if survivor == 0:\n        deadline = time.monotonic() + 30\n        while not Path(base + '.survivor-go').exists() and time.monotonic() < deadline: time.sleep(.005)\n        os._exit(0)\n    Path(base + '.survivor-pid').write_text(str(survivor))\npath.write_bytes(b'EDITED",
        );
        executable(&native.join("editor"), &editor);
        executable(&native.join("broker"), BROKER);
        let mut f = Self {
            daemon,
            base: PathBuf::new(),
            agent: String::new(),
            generation: 0,
            kind,
            instance: PathBuf::new(),
            exe,
        };
        f.spawn("first");
        f
    }

    fn spawn(&mut self, name: &str) {
        self.base = self.daemon.home.join(name);
        assert!(
            self.exe.is_file(),
            "native copied fixture exists before spawn: {}",
            self.exe.display()
        );
        let editor = format!(
            "{} fast {}",
            self.exe.parent().unwrap().join("editor").display(),
            self.base.display()
        );
        let reply = inbox::attach(
            &self.daemon,
            json!({
            "op":"start-agent", "command":format!("{} --live-segments-fixture",quote(&self.exe)), "cwd":self.daemon.home,
                "rows":40,"cols":140,"display_name":"segment-fixture",
                "agent_type":if self.kind == "codex" {"codex"} else {"claude_code"},
                "env":[[MODE,self.base.to_str().unwrap()], ["DOT_AGENT_DECK_PANE_ID",PANE],
                    ["DAD_LIVE_SEGMENTS_KIND",self.kind], ["VISUAL",editor], ["EDITOR",editor],
                    ["TMPDIR",self.daemon.home.join("tmp")], ["CODEX_HOME",self.daemon.home.join("codex")]],
            }),
        );
        assert_eq!(reply["ok"], true, "native synthetic spawn: {reply}");
        self.agent = reply["id"].as_str().unwrap().into();
        let ready = read_json(&self.path("ready"));
        self.generation = ready["generation"]
            .as_u64()
            .expect("installed synthetic generation");
        self.instance = PathBuf::from(ready["instance"].as_str().unwrap());
        assert_eq!(ready["exe"], json!(self.exe), "native exec acknowledgement");
        assert!(
            common::wait_until(common::child_boot_budget(), || {
                let screen = common::pane_snapshot_on(&self.daemon.attach_socket, &self.agent);
                let mut parser =
                    vt100::Parser::new(40.try_into().unwrap(), 140.try_into().unwrap(), 0);
                parser.process(&screen);
                parser
                    .screen()
                    .contents()
                    .contains(if self.kind == "codex" {
                        "OpenAI Codex"
                    } else {
                        "Claude Code"
                    })
            }),
            "F2 screen must be visible before any probe assertion"
        );
    }

    pub fn path(&self, suffix: &str) -> PathBuf {
        self.base.with_extension(suffix)
    }
    pub fn flag(&self, suffix: &str) {
        fs::write(self.path(suffix), "").unwrap();
    }
    pub fn setting(&self, suffix: &str, value: &str) {
        fs::write(self.path(suffix), value).unwrap();
    }
    pub fn remove(&self, suffix: &str) {
        let _ = fs::remove_file(self.path(suffix));
    }
    pub fn wait(&self, suffix: &str) {
        assert!(
            common::wait_until(common::child_boot_budget(), || self.path(suffix).exists()),
            "fixture never reached {suffix}; input {:?}",
            self.input()
        );
    }
    pub fn connect(&self) -> common::AttachInput {
        common::AttachInput::attach(&self.daemon.attach_socket, &self.agent)
    }
    pub fn input(&self) -> Vec<u8> {
        fs::read(self.path("bytes")).unwrap_or_default()
    }
    pub fn expect(&self, expected: &[u8]) {
        assert!(
            common::wait_until(WAIT, || self.input() == expected),
            "raw PTY bytes: expected {expected:?}, actual {:?}",
            self.input()
        );
    }
    pub fn stable(&self, expected: &[u8]) {
        self.command(json!({"action":"barrier"}));
        assert!(
            !common::wait_until(Duration::from_millis(250), || self.input() != expected),
            "input escaped or duplicated: expected {expected:?}, actual {:?}",
            self.input()
        );
    }
    pub fn command(&self, value: Value) -> Value {
        assert!(!self.path("command").exists());
        assert!(!self.path("response").exists());
        atomic(&self.path("command"), &value);
        let reply = read_json(&self.path("response"));
        fs::remove_file(self.path("response")).unwrap();
        reply
    }
    pub fn emit(&self, bytes: &[u8]) {
        self.command(json!({"action":"emit","bytes":bytes}));
        let needle = bytes.to_vec();
        assert!(
            common::wait_until(WAIT, || common::pane_snapshot_on(
                &self.daemon.attach_socket,
                &self.agent
            )
            .windows(needle.len())
            .any(|w| w == needle)),
            "output must reach real master reader before next input: {bytes:?}"
        );
    }

    pub fn control_for(&self, generation: u64, operation: &str) -> Value {
        inbox::attach(
            &self.daemon,
            json!({"op":"e2e-composer-control","pane_id":PANE,"generation":generation,"operation":operation}),
        )
    }
    pub fn control(&self, operation: &str) -> Value {
        let reply = self.control_for(self.generation, operation);
        assert_eq!(
            reply["ok"], true,
            "Q35-20 requires generation-bound {operation}: {reply}"
        );
        reply
    }
    pub fn epoch(&self) -> (u64, u64, u64) {
        let r = self.control("epoch");
        let e = &r["composer_epoch"];
        (
            e["epoch"].as_u64().expect("epoch number"),
            e["observed"].as_u64().expect("observed number"),
            e["last_foreign"].as_u64().expect("foreign number"),
        )
    }
    /// Request a first probe against this generation's persistent production
    /// schedule. A not-due request leaves it unchanged; an admitted attempt
    /// reaches writer-thread admission, never a direct arm/fence publication.
    pub fn probe(&self) -> Value {
        self.control("probe-request")
    }
    pub fn held(&self) -> bool {
        self.control("fence-status")["composer_fence"]["held_ops"]
            .as_u64()
            .unwrap()
            > 0
    }
    pub fn wait_held(&self) {
        assert!(
            common::wait_until(WAIT, || self.held()),
            "person's keys must be accepted behind the live segment"
        );
    }
    pub fn wait_open(&self) {
        assert!(
            common::wait_until(
                WAIT,
                || self.control("fence-status")["composer_fence"]["fenced"] == false
            ),
            "previous segment must finish before the native-publication control"
        );
    }
    pub fn page(&self) -> Publication {
        let file = File::open(self.instance.join(format!("arm-{}", self.generation))).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        // SAFETY: read-only mapping of this fixture's installed page; aligned
        // atomic reads finish before the single munmap.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(ptr, libc::MAP_FAILED);
            let p = ArmReader::at(ptr.cast(), len).unwrap().read_publication();
            assert_eq!(libc::munmap(ptr, len), 0);
            p.expect("coherent arm publication")
        }
    }
    pub fn scope(&self) -> String {
        self.control("observer-status")["composer_observer"]["scope"]
            .as_str()
            .expect("observer scope")
            .into()
    }
    pub fn screen(&self) -> String {
        let mut parser = vt100::Parser::new(40.try_into().unwrap(), 140.try_into().unwrap(), 0);
        parser.process(&common::pane_snapshot_on(
            &self.daemon.attach_socket,
            &self.agent,
        ));
        parser.screen().contents()
    }
    pub fn pause_helper(&self) -> HelperPause {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = self.path("helper-fifo");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a new private path within this fixture's sandbox.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
            .unwrap();
        self.flag("pause-helper");
        HelperPause(writer)
    }
    pub fn replace(&mut self) {
        self.flag("editor-go");
        self.flag("helper-go");
        let stopped = inbox::attach(&self.daemon, json!({"op":"stop-agent","id":self.agent}));
        assert_eq!(stopped["ok"], true, "stop fixture generation: {stopped}");
        self.spawn("successor");
    }
    pub fn snapshot(&self) -> Value {
        let r = inbox::attach(&self.daemon, json!({"op":"list-agents"}));
        r["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.agent)
            .unwrap()
            .clone()
    }
    pub fn enqueue(&self) {
        let r = inbox::attach(
            &self.daemon,
            json!({"op":"post-message","pane_id":PANE,"expected_agent_id":self.agent,"subject":"segment admission","body":"TEST-ONLY-WAITING-MAIL","origin":"seed"}),
        );
        assert_eq!(
            r["ok"], true,
            "post ordinary mail to exercise the live tick: {r}"
        );
    }
    pub fn hook_event(&self, event: &str) {
        let pid = read_json(&self.path("ready"))["pid"].as_u64().unwrap();
        let initial = fs::read(format!("/proc/{pid}/environ")).unwrap();
        let token = initial
            .split(|b| *b == 0)
            .find_map(|e| e.strip_prefix(b"DOT_AGENT_DECK_PANE_CAPABILITY="))
            .map(|v| std::str::from_utf8(v).unwrap())
            .expect("own native fixture capability");
        let request = json!({"session_id":format!("segment-session-{}",self.generation),"pane_id":PANE,"agent_id":self.agent,"token":token,"deck_build":"","event_type":event,"agent_type":if self.kind=="codex" {"codex"} else {"claude_code"},"timestamp":chrono::Utc::now(),"cwd":self.daemon.home,"metadata":{}});
        let mut stream = inbox::connect(&self.daemon, &request).into_inner();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        use std::io::Read;
        let mut discarded = Vec::new();
        stream.read_to_end(&mut discarded).unwrap();
    }
}

pub struct HelperPause(File);
impl HelperPause {
    pub fn release(mut self) {
        self.0.write_all(b"x").unwrap();
    }
}
impl Drop for HelperPause {
    fn drop(&mut self) {
        let _ = self.0.write_all(b"x");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in [
            "editor-go",
            "editor-publish-go",
            "helper-go",
            "resume-go",
            "flush-go",
            "survivor-go",
        ] {
            let _ = fs::write(self.path(suffix), "");
        }
        for n in 1..=8 {
            let _ = fs::write(self.path(&format!("resume-go-{n}")), "");
        }
        let root = self.daemon.home.join("probe-gate");
        let _ = fs::write(root.join("scheduled.release"), "");
        let _ = common::attach_request_on(
            &self.daemon.attach_socket,
            &dot_agent_deck::daemon_protocol::AttachRequest::StopAgent {
                id: self.agent.clone(),
            },
        );
    }
}

// The bridge changes only ancestry, not F2's parsing or key-loss mechanisms.
const BROKER: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
base = pathlib.Path(os.environ['DAD_LIVE_SEGMENTS_BASE'])
request = base.with_suffix('.invoke')
temporary = base.with_suffix('.invoke.tmp')
temporary.write_text(json.dumps({'path':sys.argv[-1]}))
temporary.replace(request)
response = base.with_suffix('.invoked')
while not response.exists(): time.sleep(.005)
result = json.loads(response.read_text())
response.unlink()
sys.exit(result['exit'])
"#;

/// Called by a ctor so Codex's injected `-c` arguments never reach libtest.
/// Each process is a copied synthetic executable in its own daemon HOME.
pub fn native_entry() {
    let Some(base) = std::env::var_os(MODE) else {
        return;
    };
    let base = PathBuf::from(base);
    let kind = std::env::var("DAD_LIVE_SEGMENTS_KIND").unwrap();
    let exe = std::env::current_exe().unwrap();
    let helper = PathBuf::from(std::env::var_os("VISUAL").unwrap());
    let generation = std::env::var("DOT_AGENT_DECK_EDITOR_GEN")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    fs::create_dir_all(std::env::var_os("TMPDIR").unwrap()).unwrap();
    // The common ctor removes global endpoint names. Restore just the sandbox
    // values supplied in this child's own initial exec environment to its
    // helper children; never inspect another pane or print the capability.
    let initial = fs::read("/proc/self/environ").unwrap();
    let env: Vec<(String, String)> = initial
        .split(|b| *b == 0)
        .filter_map(|e| {
            let e = std::str::from_utf8(e).ok()?;
            let (k, v) = e.split_once('=')?;
            k.starts_with("DOT_AGENT_DECK_")
                .then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    if std::env::var_os("DAD_LIVE_SEGMENTS_FAMILY_PARENT").is_some() {
        // This second copy remains the helper's real parent. Its F1 is
        // the agent's, but its PID is not A, so attribution is Unknown.
        let status = Command::new(&helper)
            .envs(env)
            .args(std::env::args_os().skip(2))
            .status()
            .unwrap();
        std::process::exit(status.code().unwrap_or(127));
    }
    let mut composer = Command::new("python3")
        .arg(exe.parent().unwrap().join("composer.py"))
        .arg(&base)
        .arg(common::contained_deck_binary())
        .arg(if kind == "codex" {
            "0.164.0"
        } else {
            "2.1.310"
        })
        .envs(env.clone())
        .env("DOT_AGENT_DECK_TEST_COMPOSER_EXTERNAL_EDIT", "1")
        .env("VISUAL", exe.parent().unwrap().join("broker"))
        .spawn()
        .unwrap();
    atomic(
        &base.with_extension("ready"),
        &json!({"pid":std::process::id(),"exe":exe,"generation":generation,"instance":helper.parent().unwrap()}),
    );
    let mut running: Option<Child> = None;
    let mut finished: Option<i32> = None;
    let mut invocation = 0;
    let mut parallel: Option<Child> = None;
    loop {
        if let Some(child) = parallel.as_mut()
            && let Some(status) = child.try_wait().unwrap()
        {
            atomic(
                &base.with_extension("parallel-result"),
                &json!({"code":status.code(),"signal":status.signal()}),
            );
            parallel = None;
        }
        if let Some(child) = running.as_mut()
            && let Some(status) = child.try_wait().unwrap()
        {
            finished = Some(status.code().unwrap_or(127));
            running = None;
            fs::write(base.with_extension("helper-exited"), "").unwrap();
            atomic(
                &base.with_extension(format!("helper-result-{invocation}")),
                &json!({"code":status.code(),"signal":status.signal()}),
            );
            fs::write(
                base.with_extension(format!("helper-exited-{invocation}")),
                "",
            )
            .unwrap();
        }
        if let Some(exit) = finished
            && (!base.with_extension("hold-return").exists()
                || base.with_extension("resume-go").exists()
                || base
                    .with_extension(format!("resume-go-{invocation}"))
                    .exists())
        {
            atomic(&base.with_extension("invoked"), &json!({"exit":exit}));
            finished = None;
        }
        if let Ok(data) = fs::read(base.with_extension("invoke")) {
            assert!(running.is_none() && finished.is_none());
            fs::remove_file(base.with_extension("invoke")).unwrap();
            let req: Value = serde_json::from_slice(&data).unwrap();
            invocation += 1;
            let target = if base.with_extension("lost-ipc").exists() {
                // Genuine unreported external editor: deliberately omit the
                // installed helper; output fallback must notice its pair.
                exe.parent().unwrap().join("editor")
            } else {
                helper.clone()
            };
            let mut command = match fs::read_to_string(base.with_extension("helper-parent"))
                .unwrap_or_default()
                .as_str()
            {
                "tool" => {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "\"$@\"; code=$?; exit \"$code\"", "fixture-tool"]);
                    command.arg(&target);
                    command
                }
                "unknown" => {
                    let mut command = Command::new(&exe);
                    command
                        .arg("--live-segments-parent")
                        .env("DAD_LIVE_SEGMENTS_FAMILY_PARENT", "1");
                    command
                }
                _ => Command::new(&target),
            };
            command.envs(env.clone());
            for (suffix, variable) in [
                ("helper-report-pad", "DOT_AGENT_DECK_E2E_HELPER_PAD"),
                ("helper-report-sndbuf", "DOT_AGENT_DECK_E2E_HELPER_SNDBUF"),
            ] {
                if let Ok(value) = fs::read_to_string(base.with_extension(suffix)) {
                    command.env(variable, value.trim());
                }
            }
            if base.with_extension("lost-ipc").exists() {
                command.arg("fast").arg(&base);
            }
            if base.with_extension("wait-unknown").exists()
                || base
                    .with_extension(format!("wait-unknown-{invocation}"))
                    .exists()
            {
                command.env("DOT_AGENT_DECK_E2E_HELPER_WAIT_ERRNO", "10");
            }
            if base.with_extension("spawn-failed").exists()
                || base
                    .with_extension(format!("spawn-failed-{invocation}"))
                    .exists()
            {
                command.env("DOT_AGENT_DECK_E2E_HELPER_SPAWN_ERRNO", "2");
            }
            if base.with_extension("pause-helper").exists() {
                command
                    .env(
                        "DOT_AGENT_DECK_E2E_HELPER_PAUSE",
                        base.with_extension("helper-fifo"),
                    )
                    .env(
                        "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT",
                        fs::read_to_string(base.with_extension("helper-pause-at"))
                            .unwrap_or_else(|_| "start".into()),
                    );
            }
            command.env(
                "DOT_AGENT_DECK_E2E_HELPER_TRACE",
                base.with_extension("helper-trace"),
            );
            let child = command.arg(req["path"].as_str().unwrap()).spawn().unwrap();
            atomic(
                &base.with_extension("helper-started"),
                &json!({"pid":child.id(),"path":req["path"]}),
            );
            running = Some(child);
            fs::write(
                base.with_extension(format!("helper-started-{invocation}")),
                "",
            )
            .unwrap();
        }
        if let Ok(data) = fs::read(base.with_extension("command")) {
            fs::remove_file(base.with_extension("command")).unwrap();
            let cmd: Value = serde_json::from_slice(&data).unwrap();
            let mut reply = json!({"ok":true});
            if cmd["action"] == "emit" {
                let bytes: Vec<u8> = serde_json::from_value(cmd["bytes"].clone()).unwrap();
                std::io::stdout().write_all(&bytes).unwrap();
                std::io::stdout().flush().unwrap();
            }
            if cmd["action"] == "input-pending" {
                // The composer is blocked in its editor broker until the
                // fixture releases the return. Inspect its actual tty queue
                // without consuming keys or allowing the return through.
                let mut pending: libc::c_int = 0;
                // SAFETY: stdin is this sandbox agent's slave terminal and
                // FIONREAD writes one c_int to the valid output pointer.
                assert_eq!(
                    unsafe { libc::ioctl(0, libc::FIONREAD, &mut pending) },
                    0,
                    "pending tty bytes: {}",
                    std::io::Error::last_os_error()
                );
                reply["pending_bytes"] = json!(pending);
            }
            if cmd["action"] == "invoke" {
                // This is real helper work by the native fixture, not an
                // injected daemon record. The path belongs to its HOME.
                let path = PathBuf::from(cmd["path"].as_str().unwrap());
                assert!(path.starts_with(std::env::var_os("HOME").unwrap()));
                atomic(&base.with_extension("invoke"), &json!({"path":path}));
            }
            if cmd["action"] == "parallel-helper" {
                // Keep the first real helper alive while a second helper
                // reaches the observer's actual record cap. Its parent is
                // this native stand-in, or an explicit tool shell.
                assert!(parallel.is_none());
                let path = PathBuf::from(cmd["path"].as_str().unwrap());
                assert!(path.starts_with(std::env::var_os("HOME").unwrap()));
                let mut command = if cmd["tool"] == true {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", "\"$@\"; code=$?; exit \"$code\"", "parallel-tool"]);
                    command.arg(&helper);
                    command
                } else {
                    Command::new(&helper)
                };
                command.envs(env.clone()).env(
                    "DOT_AGENT_DECK_E2E_HELPER_TRACE",
                    base.with_extension("parallel-trace"),
                );
                if cmd["pause"] == true {
                    command
                        .env(
                            "DOT_AGENT_DECK_E2E_HELPER_PAUSE",
                            base.with_extension("helper-fifo"),
                        )
                        .env("DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT", "start");
                }
                let child = command.arg(path).spawn().unwrap();
                reply["pid"] = json!(child.id());
                parallel = Some(child);
            }
            atomic(&base.with_extension("response"), &reply);
        }
        if composer.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Some(mut child) = running {
        let _ = child.kill();
        let _ = child.wait();
    }
    if let Some(mut child) = parallel {
        let _ = child.kill();
        let _ = child.wait();
    }
    std::process::exit(0);
}
