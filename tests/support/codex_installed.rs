//! Real interactive Codex in a deck pane the deck installs its editor helper
//! into, so the production doorbell tick checks a touched input box (PRD
//! #1666 part 3, the Codex activation's lane-2 gates `inbox/probe/041`,
//! `043` and `chain-smoke/codex/001`).
//!
//! The pane runs `codex …` as a plain argv with its agent declared, the
//! `Direct(Codex)` class; a pane whose command a shell runs is never
//! installed, so nothing here wraps it. The pane's hook capability is read
//! from that Codex process's own environment, since no shell is there to
//! record it.
//!
//! **No seam.** The build itself checks a direct Codex's box since the
//! Codex activation (Q35-29: `Direct(Codex)` in `INSTALL_ROLLOUT`, Codex's
//! `editor_probe` verified), so nothing here sets the e2e editor-probe
//! assumption, which no longer accepts a Codex release; [`checkable`] reports
//! `SKIP:` on a build that does not.
//!
//! **A project folder that doesn't hold the Codex home, as a user's.** Codex
//! 0.164.0 puts the file it hands an editor in the first of `$CODEX_HOME`,
//! `~/.codex` and `<cwd>/.codex` that its own sandbox can't write to
//! (`tui/src/external_editor.rs`, `editor_directory`), and the deck checks
//! only `$CODEX_HOME/editor`. A user's `~/.codex` is outside the project
//! Codex works in, so the first candidate wins. The harness's HOME is inside
//! the fixture's working directory, where `workspace-write` lets Codex
//! write, so every Codex here runs in [`project_dir`], a folder of that
//! working directory that doesn't hold HOME, and the files it is asked to
//! write go there. (A Codex home inside a folder its sandbox can write is a
//! case the deck can't check today; `docs/develop/doorbell-admission.md`.)
//!
//! **A fresh `CODEX_HOME`, as a new user's.** Each deck's Codex home is new,
//! so its pane is Codex's first start there, and Codex rewrites its own
//! `config.toml` (a key-binding source) a few seconds after drawing its
//! composer. The deck's key-binding guard accepts that bookkeeping step, so
//! no warm-up start precedes the pane under test (the warm-up the first
//! version of this harness made, `steady_codex_home`, is gone with the need
//! for it, and with it the sandbox daemon and wrapper each warm-up start left
//! behind).
#![allow(dead_code)] // Each test file uses its own subset.

use crate::common;
use crate::inbox::{self, attach, hook, request};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub const BUDGET: Duration = Duration::from_secs(120);
pub const PANE: &str = "codex-installed";
pub const LOG: &str = "codex-installed.log";

/// The folder every Codex here works in (module doc): `project` in the
/// fixture's working directory, created on first use. It is inside the
/// fixture's git repository, whose trust the harness records, and it doesn't
/// hold the deck's HOME.
pub fn project_dir(deck: &common::TuiDeck) -> PathBuf {
    let dir = deck.workdir().join("project");
    std::fs::create_dir_all(&dir).expect("the Codex project folder");
    dir
}

/// The Codex command every installed pane runs: a plain argv (no quotes or
/// operators a shell would read), the cheap test model, its workspace
/// sandbox and no approval prompts.
pub fn command() -> String {
    format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never \
         -c sandbox_workspace_write.network_access=true -c model_reasoning_effort=low",
        common::codex_test_model()
    )
}

/// The installed Codex release, read from `codex --version`.
pub fn installed_release() -> Result<String, String> {
    crate::live::installed_codex_release()
}

/// Whether this build itself checks a direct Codex's box: `Direct(Codex)`
/// in the rollout and Codex's editor probe verified (the activation).
pub fn activated() -> bool {
    use dot_agent_deck::composer_probe::install::{INSTALL_ROLLOUT, PaneClass, ProbeAgent};
    use dot_agent_deck::doorbell_activation::{CODEX_FACTS, Measurement};
    INSTALL_ROLLOUT.contains(&PaneClass::Direct(ProbeAgent::Codex))
        && matches!(CODEX_FACTS.editor_probe, Measurement::Verified(_))
}

/// The preflight: real Codex runs here, its release is one the deck latches,
/// Codex's facts ring it, and this build checks a direct Codex's box.
/// Otherwise the reason, so the test reports `SKIP:` (UNVERIFIED under
/// `DOT_AGENT_DECK_REQUIRE_REAL_E2E`).
pub fn checkable() -> Result<(), String> {
    common::check_codex_available()?;
    crate::live::codex_facts_admit_installed_release()?;
    let release = installed_release()?;
    if !activated() {
        return Err(format!(
            "this build does not check a direct Codex's box (Codex is not activated); Codex {release}"
        ));
    }
    eprintln!("Codex {release}: this build checks a direct Codex's box, with no assumption");
    Ok(())
}

/// One deck-written or person-written op, from the daemon's own ledger.
pub use crate::installed::Op;

pub struct Installed {
    pub deck: common::TuiDeck,
    pub pane: inbox::Pane,
    /// A Codex process of the pane (npm's `node …/codex` entrypoint or the
    /// native it spawns), the one whose environment carried the capability.
    pub pid: u32,
}

impl Installed {
    /// A deck with `extra` set for the deck and its daemon, whose one pane
    /// runs [`command`] in the fixture's working directory, with its own
    /// fresh `CODEX_HOME` holding the deck's hooks trusted and the directory
    /// trusted. Waits for the deck to latch the installed release from the
    /// pane's own header and for Codex's composer.
    pub fn new(extra: &[(&str, &str)]) -> Self {
        let release = installed_release().expect("the installed release");
        let mut builder = common::TuiDeck::builder();
        for (key, value) in extra {
            builder = builder.with_env(*key, *value);
        }
        let deck = builder
            .with_pty_size(180, 50)
            .with_imported_codex_credentials()
            .with_env("DOT_AGENT_DECK_LOG", LOG)
            .with_env(
                "RUST_LOG",
                "info,pane_write=trace,dot_agent_deck::doorbell=debug,composer_probe=debug",
            )
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "2000")
            // A `dot-agent-deck` the model runs by name (the doorbell's
            // `inbox fetch`) is the build under test, not one on the host.
            .with_env("PATH", common::path_with_binary_dir())
            .launch_with_fixture("minimal");
        assert!(
            deck.wait_for_grid_string_within("No active agents", BUDGET),
            "scratch dashboard must start before launching Codex: {}",
            deck.snapshot_grid()
        );
        crate::live::prepare(&deck);
        let reply = start_pane(&deck);
        let agent = reply["id"].as_str().unwrap().to_string();
        let found = std::cell::RefCell::new(None);
        assert!(
            common::wait_until(BUDGET, || {
                *found.borrow_mut() = codex_process(&deck, &agent);
                found.borrow().is_some()
            }),
            "the pane's Codex process must start"
        );
        let (pid, token) = found.into_inner().unwrap();
        let pane = inbox::Pane {
            id: PANE.into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let codex = Self { deck, pane, pid };
        let latched = format!("release={release}");
        assert!(
            common::wait_until(BUDGET, || codex.log().lines().any(|line| {
                line.contains("pane release") && line.split_whitespace().any(|w| w == latched)
            })),
            "the deck latched the installed release {release} from the pane's header; {}",
            codex.evidence()
        );
        assert!(
            common::wait_until(BUDGET, || codex.screen().contains("? for shortcuts")),
            "Codex must draw its composer; {}",
            codex.evidence()
        );
        codex
    }

    /// A Codex process's environment, as the deck spawned the pane.
    pub fn environ(&self) -> Vec<(String, String)> {
        crate::installed::environ_of(self.pid)
    }

    pub fn env(&self, name: &str) -> Option<String> {
        self.environ()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The pane was installed: Codex's `VISUAL` and `EDITOR` name the deck's
    /// helper and it has a generation, and the deck bound the native Codex
    /// below the pane (its editor-probe permission is `Armable`).
    pub fn assert_installed(&self) {
        let helper = dot_agent_deck::composer_probe::instance::HELPER_NAME;
        for name in ["VISUAL", "EDITOR"] {
            let value = self.env(name).unwrap_or_default();
            assert!(
                value.ends_with(&format!("/{helper}")),
                "an installed pane's {name} is the deck's helper, got {value:?}; {}",
                self.evidence()
            );
        }
        assert!(
            self.env(dot_agent_deck::composer_probe::install::GENERATION_ENV)
                .is_some(),
            "an installed pane names its generation"
        );
    }

    /// The installed pane's editor-probe permission settled `Armable` (the
    /// observer's `editor probe permission settled` line for this agent).
    pub fn wait_armable(&self) {
        assert!(
            common::wait_until(BUDGET, || self.log().lines().any(|l| {
                l.contains("editor probe permission settled")
                    && l.contains(&self.pane.agent)
                    && l.contains("Armable")
            })),
            "the deck must bind the pane's Codex and settle its permission Armable; {}",
            self.evidence()
        );
    }

    /// Type `bytes` into the pane as a client attached to it does.
    pub fn input(&self) -> common::AttachInput {
        common::AttachInput::attach(self.deck.attach_socket_path(), &self.pane.agent)
    }

    pub fn listing(&self) -> Value {
        let mut req = request(&self.pane, "inbox_list");
        req["all"] = json!(false);
        hook(&self.deck, req)
    }

    pub fn message(&self, id: &str) -> Value {
        self.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub fn record(&self) -> Value {
        attach(&self.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.pane.agent)
            .unwrap()
            .clone()
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join(LOG)).unwrap_or_default()
    }

    pub fn ops(&self, since: usize) -> Vec<Op> {
        crate::installed::ops_in(self.log().get(since..).unwrap_or(""))
    }

    pub fn checks(&self, since: usize) -> Vec<String> {
        crate::installed::checks_in(self.log().get(since..).unwrap_or(""))
    }

    pub fn empty_reads(&self, since: usize) -> usize {
        self.log()
            .get(since..)
            .unwrap_or("")
            .lines()
            .filter(|line| line.contains("empty read applied"))
            .count()
    }

    /// The pane's screen, as a fresh parser of its size reads its snapshot.
    pub fn parser(&self) -> vt100::Parser {
        let record = self.record();
        let raw = common::pane_snapshot_on(self.deck.attach_socket_path(), &self.pane.agent);
        let mut parser = dot_agent_deck::vt_grid::parser(
            record["rows"].as_u64().unwrap() as u16,
            record["cols"].as_u64().unwrap() as u16,
            0,
        );
        parser.process(&raw);
        parser
    }

    pub fn screen(&self) -> String {
        self.parser().screen().contents()
    }

    /// The input box's text: the last row whose first glyph is Codex's `›`
    /// prompt, without the glyph and without dim cells (the placeholder
    /// Codex draws in an empty box). `None` while no such row is drawn, as
    /// while the deck's check or an editor runs.
    pub fn composer(&self) -> Option<String> {
        composer_of(self.parser().screen())
    }

    pub fn expect_composer(&self, expected: &str) {
        assert!(
            common::wait_until(Duration::from_secs(15), || self.composer().as_deref()
                == Some(expected)),
            "Codex must draw exact composer {expected:?}, got {:?}; {}",
            self.composer(),
            self.evidence()
        );
    }

    pub fn evidence(&self) -> String {
        common::redact_credentials_for_output(&format!(
            "inbox={}\nrecord={}\nPane:\n{}\nGrid:\n{}\nLog (last 300 lines but the ring's per-tick `not admitted`):\n{}",
            self.listing(),
            self.record(),
            self.screen(),
            self.deck.snapshot_grid(),
            crate::installed::tail(
                &self
                    .log()
                    .lines()
                    .filter(|l| !l.contains("doorbell ring not admitted"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                300
            )
        ))
    }

    /// Post a task asking Codex to append one uniquely named line to `name`
    /// in the working directory.
    pub fn post(&self, name: &str) -> String {
        inbox::enqueue(&self.deck, &self.pane, &self.directive(name))
    }

    pub fn directive(&self, name: &str) -> String {
        format!(
            "Run exactly this shell command once: printf '%s\\n' 'inbox-action-{name}' >> '{}'. Then reply DONE-{name} and end your turn. Do this once for this message id.",
            project_dir(&self.deck).join(name).display()
        )
    }

    /// The task `id` was acted on once and acknowledged after one hand-over.
    pub fn done(&self, id: &str, name: &str) {
        let path = project_dir(&self.deck).join(name);
        assert!(
            common::wait_until(BUDGET, || std::fs::read_to_string(&path)
                .is_ok_and(|text| text.ends_with('\n'))),
            "real Codex must act on {id}; {}",
            self.evidence()
        );
        assert!(
            common::wait_until(BUDGET, || self.message(id)["state"] == "acknowledged"),
            "message must be acknowledged; {}",
            self.evidence()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("inbox-action-{name}\n"),
            "one append proves one action"
        );
        let message = self.message(id);
        assert_eq!(message["deliveries"], 1, "one hand-over: {message}");
    }

    /// Wait for Codex's turn to end.
    pub fn idle(&self) {
        assert!(
            common::wait_until(BUDGET, || self.record()["live"]["status"] == "Idle"),
            "the turn must finish; {}",
            self.evidence()
        );
    }
}

/// Start [`command`] in `deck`'s one Codex pane: the agent declared, its own
/// `CODEX_HOME` the sandbox's, in [`project_dir`].
fn start_pane(deck: &common::TuiDeck) -> Value {
    let reply = attach(
        deck,
        json!({"op":"start-agent", "command":command(),
        "agent_type":"codex", "cwd":project_dir(deck), "rows":40,"cols":140,
        "display_name":PANE, "env":[
            ["CODEX_HOME",deck.home_dir().join(".codex")],
            ["DOT_AGENT_DECK_PANE_ID",PANE]]}),
    );
    assert_eq!(reply["ok"], true, "installed Codex spawn: {reply}");
    reply
}

/// [`Installed::composer`] on a screen.
pub fn composer_of(screen: &vt100::Screen) -> Option<String> {
    let (rows, cols) = dot_agent_deck::vt_grid::size(screen);
    (0..rows).rev().find_map(|row| {
        let mut text = String::new();
        let mut seen_glyph = false;
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let contents = cell.contents();
            if !seen_glyph {
                if contents.trim().is_empty() {
                    continue;
                }
                if contents != "\u{203a}" {
                    return None;
                }
                seen_glyph = true;
                continue;
            }
            if cell.dim() {
                continue;
            }
            text.push_str(if contents.is_empty() { " " } else { contents });
        }
        seen_glyph.then(|| text.trim().to_string())
    })
}

/// A Codex process of `agent` under `deck`: its argv names a `codex`
/// executable or script and it is not the wrapper's `codex app-server`
/// trust probe, and its environment carries this deck's HOME and
/// the agent's id. With it, the pane's hook capability from that environment.
fn codex_process(deck: &common::TuiDeck, agent: &str) -> Option<(u32, String)> {
    let home = format!("HOME={}", deck.home_dir().display());
    let id = format!("DOT_AGENT_DECK_AGENT_ID={agent}");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmd.split(|c| *c == 0).collect();
        let named = args
            .iter()
            .take(2)
            .any(|a| String::from_utf8_lossy(a).ends_with("codex"));
        // The deck's wrapper runs `codex app-server` once to read its hooks'
        // trust; that short-lived process is not the pane's Codex.
        if !named || args.iter().any(|a| *a == b"app-server") {
            continue;
        }
        let env = std::fs::read(entry.path().join("environ")).unwrap_or_default();
        let vars: Vec<&[u8]> = env.split(|c| *c == 0).collect();
        if !vars.contains(&home.as_bytes()) || !vars.contains(&id.as_bytes()) {
            continue;
        }
        let token = vars.iter().find_map(|v| {
            let v = String::from_utf8_lossy(v);
            v.strip_prefix(&format!(
                "{}=",
                dot_agent_deck::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY
            ))
            .map(str::to_string)
        })?;
        if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some((pid, token));
        }
    }
    None
}

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
