//! Native stand-in with an exec/readiness acknowledgement and atomic mailboxes.
//! The sole assumed production interface is documented at `control_for`.
use crate::{common, inbox, installation};
use dot_agent_deck::composer_probe::arm::{ArmReader, Publication};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const ENTRY: &str = "probe_021_owned_helpers_settle_once_after_the_arm_load";
const MODE: &str = "DAD_SETTLEMENT_AGENT";
const BASE: &str = "DAD_SETTLEMENT_BASE";

fn atomic_json(path: &Path, value: &Value) {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    temporary
        .write_all(&serde_json::to_vec(value).unwrap())
        .unwrap();
    temporary.persist(path).unwrap();
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

pub struct Fixture {
    pub daemon: common::DaemonProc,
    pub base: PathBuf,
    pub repo: PathBuf,
    pub generation: u64,
    pane: String,
    token: String,
    instance: PathBuf,
    sentinel: PathBuf,
    exe: PathBuf,
}

impl Fixture {
    pub fn start() -> Self {
        let daemon = common::spawn_daemon_serve_with_env(
            None,
            "0",
            &[("DOT_AGENT_DECK_E2E_ASSUME_EDITOR_PROBE", "claude@2.1.310")],
        );
        let native = daemon.home.join("native");
        fs::create_dir(&native).unwrap();
        let exe = native.join("claude");
        fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        let base = daemon.home.join("fixture");
        let sentinel = daemon.home.join("editor-opened");
        let editor = daemon.home.join("sentinel-editor");
        executable(
            &editor,
            &format!(
                "#!/bin/sh\nprintf 'opened\\n' >> {}\nprintf 'settlement fixture commit\\n' > \"$1\"\ncase \"$1\" in *COMMIT_EDITMSG) exit 0;; *) exit 37;; esac\n",
                installation::quote(&sentinel),
            ),
        );
        let tmp = daemon.home.join("tmp");
        let uid = fs::metadata(&daemon.home).unwrap().uid();
        let exports = tmp.join(format!("claude-{uid}"));
        fs::create_dir_all(&exports).unwrap();
        fs::set_permissions(&exports, fs::Permissions::from_mode(0o700)).unwrap();
        let export = exports.join("claude-prompt-settlement.md");
        fs::write(&export, "").unwrap();
        let repo = daemon.home.join("tool-repo");
        fs::create_dir(&repo).unwrap();
        let started = inbox::attach(
            &daemon,
            json!({
                "op": "start-agent",
                "command": format!("{} --exact {ENTRY} --nocapture --test-threads=1", installation::quote(&exe)),
                "cwd": daemon.home, "rows": 40, "cols": 140,
                "env": [[MODE, "loop"], [BASE, base.to_str().unwrap()],
                    ["DOT_AGENT_DECK_PANE_ID", "settlement-agent"],
                    ["DAD_SETTLEMENT_PANE", "settlement-agent"],
                    ["DAD_SETTLEMENT_EXPORT", export.to_str().unwrap()],
                    ["DAD_SETTLEMENT_REPO", repo.to_str().unwrap()],
                    ["TMPDIR", tmp.to_str().unwrap()], ["VISUAL", editor.to_str().unwrap()],
                    ["EDITOR", editor.to_str().unwrap()]],
            }),
        );
        assert_eq!(started["ok"], true, "native stand-in spawn: {started}");
        assert!(
            common::wait_until(common::child_boot_budget(), || base
                .with_extension("ready")
                .exists()),
            "native exec readiness: {started}; screen: {}",
            String::from_utf8_lossy(&common::pane_snapshot_on(
                &daemon.attach_socket,
                started["id"].as_str().unwrap_or("")
            ))
        );
        let report = installation::read_json(&base.with_extension("ready"));
        assert_eq!(report["exe"], json!(exe), "acknowledge exec, not just fork");
        let helper = PathBuf::from(report["helper"].as_str().expect("installed VISUAL"));
        assert_eq!(helper.file_name().unwrap(), "dot-agent-deck-editor");
        let generation: u64 = report["generation"].as_str().unwrap().parse().unwrap();
        let pane = report["pane"].as_str().unwrap().to_string();
        // Linux exposes the initial exec environment even after the copied
        // harness's ctor removes its process-global endpoint variables. Read
        // only our acknowledged native child's injected capability, never a
        // live pane's, and never include its value in diagnostic output.
        let pid = report["pid"].as_u64().unwrap();
        let initial = fs::read(format!("/proc/{pid}/environ")).unwrap();
        let prefix = b"DOT_AGENT_DECK_PANE_CAPABILITY=";
        let token = initial
            .split(|byte| *byte == 0)
            .find_map(|entry| entry.strip_prefix(prefix))
            .map(|value| String::from_utf8(value.to_vec()).unwrap())
            .expect("the native fixture has its own injected capability");
        assert_eq!(token.len(), 64, "a complete capability, never logged");
        Fixture {
            daemon,
            base,
            repo,
            generation,
            pane,
            token,
            instance: helper.parent().unwrap().to_owned(),
            sentinel,
            exe,
        }
    }

    /// OQ10's e2e-only generation-bound attach verb. No reply state is assumed:
    /// the assertion reads the actual shared arm page. The coder may rename
    /// this spelling; the semantic contract is publish/withdraw, no input.
    pub fn control_for(&self, generation: u64, operation: &str) -> Value {
        inbox::attach(
            &self.daemon,
            json!({"op": "e2e-composer-control",
            "pane_id": self.pane, "generation": generation, "operation": operation}),
        )
    }

    fn control(&self, operation: &str) {
        let reply = self.control_for(self.generation, operation);
        assert_eq!(
            reply["ok"], true,
            "Q35-14b must serve the generation-bound {operation} control: {reply}"
        );
    }

    pub fn publish(&self) {
        self.control("publish");
    }
    pub fn withdraw(&self) {
        self.control("withdraw");
    }

    fn page(&self) -> Publication {
        let file = File::open(self.instance.join(format!("arm-{}", self.generation))).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        // SAFETY: map the existing page read-only, keep it mapped throughout the
        // aligned atomic read, and unmap it exactly once afterwards.
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
            let result = ArmReader::at(ptr.cast(), len).unwrap().read_publication();
            assert_eq!(libc::munmap(ptr, len), 0);
            result.expect("a stable published arm page")
        }
    }

    pub fn expect_count(&self, count: u32, why: &str) {
        assert_eq!(self.page().count, count, "{why}");
    }

    pub fn wait_count(&self, count: u32, why: &str) {
        assert!(
            common::wait_until(common::child_boot_budget(), || self.page().count == count),
            "{why}: last publication {:?}",
            self.page()
        );
    }

    pub fn expect_count_stays(&self, count: u32) {
        // Cover four 50 ms scan opportunities after the first helper exited.
        assert!(
            !common::wait_until(Duration::from_millis(200), || self.page().count != count),
            "late IPC, marker or exit evidence must not spend the later arm"
        );
    }

    pub fn expect_editor_opens(&self, count: usize) {
        let actual = fs::read_to_string(&self.sentinel).unwrap_or_default();
        assert_eq!(actual, "opened\n".repeat(count), "real-editor sentinel");
    }

    pub fn no_editor(&self) {
        self.expect_editor_opens(0);
    }

    fn submit(&self, mut request: Value) {
        assert!(!self.base.with_extension("request").exists());
        assert!(!self.base.with_extension("result").exists());
        request["trace"] = json!(self.base.with_extension("trace"));
        // Keep the copied binary's pre-main endpoint scrub. Only children get
        // this contained daemon's endpoints, with no process-global mutation.
        request["socket"] = json!(self.daemon.hook_socket);
        request["pane"] = json!(self.pane);
        request["token"] = json!(self.token);
        atomic_json(&self.base.with_extension("request"), &request);
    }

    pub fn result(&self) -> Value {
        let path = self.base.with_extension("result");
        let value = installation::read_json(&path);
        fs::remove_file(path).unwrap();
        value
    }

    pub fn run(&self, request: Value) -> Value {
        self.submit(request);
        self.result()
    }

    pub fn paused_owned(&self, marker_only: bool) -> Paused {
        let fifo = self.base.with_extension("pause");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: name is a valid C path; create a private FIFO in our fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // O_RDWR avoids blocking this thread before the helper opens its reader.
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        let paused = Paused {
            writer,
            trace: self.base.with_extension("trace"),
            released: false,
        };
        self.submit(json!({"kind": "owned", "pause": fifo, "marker_only": marker_only}));
        paused
    }

    pub fn replace_agent(&self) {
        let updated = self.exe.with_file_name("updated");
        fs::copy(&self.exe, &updated).unwrap();
        fs::remove_file(self.base.with_extension("ready")).unwrap();
        self.submit(json!({"kind": "exec", "executable": updated}));
        let report = installation::read_json(&self.base.with_extension("ready"));
        assert_eq!(
            report["exe"],
            json!(updated),
            "replacement exec acknowledged"
        );
        assert_eq!(
            report["generation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            self.generation
        );
    }
}

pub struct Paused {
    writer: File,
    trace: PathBuf,
    released: bool,
}

impl Paused {
    pub fn wait_started(&self) {
        assert!(
            common::wait_until(common::child_boot_budget(), || fs::read_to_string(
                &self.trace
            )
            .unwrap_or_default()
            .contains("start")),
            "the real helper must acknowledge its pre-load pause"
        );
    }
    pub fn resume(mut self) {
        self.writer.write_all(b"x").unwrap();
        self.released = true;
    }
}

impl Drop for Paused {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.writer.write_all(b"x");
        }
    }
}

fn self_command(exe: &Path) -> Command {
    let mut command = Command::new(exe);
    command.args(["--exact", ENTRY, "--nocapture", "--test-threads=1"]);
    command
}

fn invoke(request: &Value) -> Output {
    let helper = std::env::var_os("VISUAL").unwrap();
    let export = std::env::var_os("DAD_SETTLEMENT_EXPORT").unwrap();
    let mut command = Command::new(helper);
    helper_env(&mut command, request);
    command.arg(export).env(
        "DOT_AGENT_DECK_E2E_HELPER_TRACE",
        request["trace"].as_str().unwrap(),
    );
    if let Some(path) = request["pause"].as_str() {
        command
            .env("DOT_AGENT_DECK_E2E_HELPER_PAUSE", path)
            .env("DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT", "start");
    }
    if request["marker_only"] == true {
        command.env(
            "DOT_AGENT_DECK_SOCKET",
            Path::new(&std::env::var_os("HOME").unwrap()).join("absent.sock"),
        );
    }
    command.output().unwrap()
}

fn helper_env(command: &mut Command, request: &Value) {
    command
        .env("DOT_AGENT_DECK_SOCKET", request["socket"].as_str().unwrap())
        .env("DOT_AGENT_DECK_PANE_ID", request["pane"].as_str().unwrap())
        .env(
            "DOT_AGENT_DECK_PANE_CAPABILITY",
            request["token"].as_str().unwrap(),
        );
}

fn handle(request: &Value) -> Output {
    let exe = std::env::current_exe().unwrap();
    match request["kind"].as_str().unwrap() {
        "owned" => invoke(request),
        "nested" | "updated" => {
            let target = if request["kind"] == "updated" {
                let path = exe.with_file_name("next-release");
                fs::copy(&exe, &path).unwrap();
                path
            } else { exe };
            // Bash is a real ancestor for the nested case, then execs F1:
            // the kernel tree after exec must still classify it Unknown.
            let mut command = Command::new("/bin/bash");
            command.args(["--noprofile", "--norc", "-c", "exec \"$@\"", "tool-shell"])
                .arg(target).args(["--exact", ENTRY, "--nocapture", "--test-threads=1"])
                .env(MODE, "invoke").env("DAD_SETTLEMENT_INVOCATION", request.to_string());
            command.output().unwrap()
        }
        "tool" => {
            let mut command = Command::new("/bin/bash");
            helper_env(&mut command, request);
            command.args(["--noprofile", "--norc", "-c",
            "\"$VISUAL\" \"$DAD_SETTLEMENT_EXPORT\"; result=$?; exit \"$result\""]).output().unwrap()
        }
        "orphan" => {
            let mut command = Command::new("python3");
            helper_env(&mut command, request);
            command.arg("-c").arg(
            "import os,sys,subprocess,time\nr,w=os.pipe()\np=os.fork()\nif p:\n os.close(w); result=os.read(r,32); os.waitpid(p,0); sys.exit(int(result))\nos.close(r)\nparent=os.getpid()\np=os.fork()\nif p: os._exit(0)\nwhile os.getppid()==parent: time.sleep(.001)\nresult=subprocess.run([os.environ['VISUAL'],os.environ['DAD_SETTLEMENT_EXPORT']])\nos.write(w,str(result.returncode).encode()); os.close(w); os._exit(0)"
        ).output().unwrap()
        }
        "git" => Command::new("/bin/bash").args(["--noprofile", "--norc", "-c",
            "source \"$CLAUDE_ENV_FILE\" && git init \"$DAD_SETTLEMENT_REPO\" && git -C \"$DAD_SETTLEMENT_REPO\" -c user.name=Fixture -c user.email=fixture@example.invalid commit --allow-empty"
        ]).output().unwrap(),
        kind => panic!("unknown trusted fixture request {kind}"),
    }
}

/// Called only by the 021 function in a copy of the native test executable.
pub fn agent_process() -> bool {
    let Ok(mode) = std::env::var(MODE) else {
        return false;
    };
    if mode == "invoke" {
        let request =
            serde_json::from_str(&std::env::var("DAD_SETTLEMENT_INVOCATION").unwrap()).unwrap();
        let output = invoke(&request);
        std::io::stdout().write_all(&output.stdout).unwrap();
        std::io::stderr().write_all(&output.stderr).unwrap();
        std::process::exit(output.status.code().unwrap_or(127));
    }
    let base = PathBuf::from(std::env::var_os(BASE).unwrap());
    // SAFETY: stdin is the fixture's PTY. Put its own terminal into raw mode
    // so the input capture records actual bytes, including control keys.
    unsafe {
        let mut term = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &mut term), 0);
        libc::cfmakeraw(&mut term);
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &term), 0);
    }
    let input = base.with_extension("input");
    fs::write(&input, "").unwrap();
    let mut capture = OpenOptions::new().append(true).open(input).unwrap();
    print!(
        "\x1b[2J\x1b[H\x1b[2;12HClaude Code v2.1.310\r\n\x1b[25;1H{}\x1b[26;1H❯ \x1b[2mTry \"fix the bug\"\x1b[22m\x1b[27;1H{}\x1b[28;3H? for shortcuts\x1b[26;3H",
        "─".repeat(140),
        "─".repeat(140)
    );
    std::io::stdout().flush().unwrap();
    atomic_json(
        &base.with_extension("ready"),
        &json!({
            "pid": std::process::id(), "exe": std::env::current_exe().unwrap(),
            "helper": std::env::var("VISUAL").unwrap(),
            "pane": std::env::var("DAD_SETTLEMENT_PANE").unwrap(),
            "generation": std::env::var("DOT_AGENT_DECK_EDITOR_GEN").unwrap(),
        }),
    );
    loop {
        let path = base.with_extension("request");
        if let Ok(bytes) = fs::read(&path) {
            fs::remove_file(path).unwrap();
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            if request["kind"] == "exec" {
                panic!(
                    "replacement exec failed: {}",
                    self_command(Path::new(request["executable"].as_str().unwrap())).exec()
                );
            }
            let output = handle(&request);
            atomic_json(
                &base.with_extension("result"),
                &json!({
                    "exit": output.status.code(), "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                }),
            );
        }
        let mut poll = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialized pollfd, bounded wait, no retained pointer.
        if unsafe { libc::poll(&mut poll, 1, 10) } > 0 {
            let mut bytes = [0; 4096];
            let count = std::io::stdin().read(&mut bytes).unwrap();
            if count == 0 {
                break;
            }
            capture.write_all(&bytes[..count]).unwrap();
        }
    }
    true
}
