//! Real interactive Claude Code in a deck pane the deck installs its editor
//! helper into, so the production doorbell tick checks a touched input box
//! (PRD #1666 part 3, Q35-28's lane-2 gates).
//!
//! The pane runs `claude …` as a plain argv, the `Direct(Claude)` class the
//! activation rolls out; a pane whose command a shell or a wrapper runs is
//! not installed, so nothing here wraps it. The pane's hook capability is
//! read from that Claude process's own environment, since no shell is there
//! to record it. Nothing here sets the e2e editor-probe assumption: these
//! tests run only where the build itself rolls installation out to
//! `Direct(Claude)` and verifies the installed release's editor probe
//! ([`checkable`]), and are otherwise UNVERIFIED (`SKIP:`), never green.
#![allow(dead_code)] // Each test file uses its own subset.

use crate::common;
use crate::inbox::{self, attach, hook, request};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// The Claude Code command every installed pane runs: a plain argv.
pub const CLAUDE: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write Task Agent";

pub const SUBJECT: &str = "live inbox task";
pub const BUDGET: Duration = Duration::from_secs(120);
pub const PANE: &str = "installed-haiku";
pub const LOG: &str = "installed-live.log";

/// The installed Claude Code release, read from `claude --version`.
pub fn installed_release() -> Result<String, String> {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .map_err(|e| format!("`claude --version` did not run: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.split_whitespace()
        .next()
        .filter(|release| semver::Version::parse(release).is_ok())
        .map(str::to_string)
        .ok_or_else(|| format!("`claude --version` printed no release: {text:?}"))
}

/// The preflight: this build installs `Direct(Claude)` panes, and Claude
/// Code's facts are ones the doorbell types into with a verified editor
/// probe, so the deck checks the installed release's box whatever its
/// version. Otherwise the reason, so the test reports `SKIP:` (UNVERIFIED
/// under `DOT_AGENT_DECK_REQUIRE_REAL_E2E`).
pub fn checkable() -> Result<(), String> {
    use dot_agent_deck::composer_probe::install::{INSTALL_ROLLOUT, PaneClass, ProbeAgent};
    use dot_agent_deck::doorbell_activation::{
        AGENT_FACTS, Activation, CLAUDE_FACTS, LatchedRelease, Measurement, activation,
    };
    use dot_agent_deck::event::AgentType;
    common::check_claude_available()?;
    if !INSTALL_ROLLOUT.contains(&PaneClass::Direct(ProbeAgent::Claude)) {
        return Err(format!(
            "this build installs no Direct(Claude) pane (INSTALL_ROLLOUT = {INSTALL_ROLLOUT:?}): \
             the deck checks no Claude input box before activation"
        ));
    }
    let release = installed_release()?;
    let version = semver::Version::parse(&release).map_err(|e| e.to_string())?;
    if !version.pre.is_empty() {
        return Err(format!(
            "Claude Code {release} is a pre-release, whose start-up header the deck does not \
             latch (`unknown_release`): the deck checks no box and types no doorbell"
        ));
    }
    if !matches!(CLAUDE_FACTS.editor_probe, Measurement::Verified(_)) {
        return Err(format!(
            "Claude Code's editor probe is {:?}, not verified: the deck checks no box",
            CLAUDE_FACTS.editor_probe
        ));
    }
    if !matches!(
        activation(
            &AGENT_FACTS,
            &AgentType::ClaudeCode,
            &LatchedRelease::Known(version)
        ),
        Activation::Go { .. }
    ) {
        return Err(format!(
            "the doorbell does not type into Claude Code {release}"
        ));
    }
    eprintln!("Claude Code release under test: {release} (installed, on Claude Code's facts)");
    Ok(())
}

/// One deck-written or person-written op, from the daemon's own ledger
/// (`composer_probe`'s `an op moved the epoch` line, which every op that
/// reaches the pane's writer and isn't an exempt terminal reply logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub deck: bool,
    pub len: usize,
}

pub struct Installed {
    pub deck: common::TuiDeck,
    pub pane: inbox::Pane,
    /// The Claude Code process the pane runs.
    pub pid: u32,
}

impl Installed {
    /// A deck whose one pane runs `command` (a plain `claude …` argv for an
    /// installed pane), with `extra` set for the deck and its daemon.
    pub fn launch(command: &str, extra: &[(&str, &str)]) -> Self {
        let mut builder = common::TuiDeck::builder();
        for (key, value) in extra {
            builder = builder.with_env(*key, *value);
        }
        let deck = builder
            .with_pty_size(180, 50)
            .with_imported_claude_credentials()
            .with_claude_trust_workdir()
            .with_env("DOT_AGENT_DECK_LOG", LOG)
            .with_env(
                "RUST_LOG",
                "info,pane_write=trace,dot_agent_deck::doorbell=debug,composer_probe=debug",
            )
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "2000")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "2000")
            .with_continue_session(PANE, command)
            .launch_with_fixture("minimal");
        deck.wait_for_string(PANE);
        let record = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(BUDGET, || {
                *record.borrow_mut() = attach(&deck, json!({"op":"list-agents"}))["agent_records"]
                    .as_array()
                    .and_then(|rs| rs.iter().find(|r| r["display_name"] == PANE))
                    .cloned()
                    .unwrap_or(Value::Null);
                record.borrow()["pane_id_env"].is_string()
            }),
            "Claude pane must register"
        );
        let record = record.into_inner();
        let agent = record["id"].as_str().unwrap().to_string();
        let found = std::cell::RefCell::new(None);
        assert!(
            common::wait_until(BUDGET, || {
                *found.borrow_mut() = claude_process(&deck, &agent);
                found.borrow().is_some()
            }),
            "the pane's Claude Code process must start"
        );
        let (pid, token) = found.into_inner().unwrap();
        let pane = inbox::Pane {
            id: record["pane_id_env"].as_str().unwrap().into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self { deck, pane, pid };
        result.deck.wait_for_string("Claude Code v");
        result
    }

    /// [`Self::launch`] of [`CLAUDE`], waiting for its native waiter, and
    /// for the deck to have latched the installed release (what
    /// `claude --version` says) from the pane's own start-up header.
    pub fn new(extra: &[(&str, &str)]) -> Self {
        let claude = Self::launch(CLAUDE, extra);
        assert!(
            common::wait_until(BUDGET, || claude.listing()["armed"].as_u64().unwrap_or(0)
                > 0),
            "native SessionStart waiter must arm; {}",
            claude.evidence()
        );
        let release = installed_release().expect("the installed release");
        let latched = format!("release={release}");
        assert!(
            common::wait_until(BUDGET, || claude.log().lines().any(|line| {
                line.contains("pane release") && line.split_whitespace().any(|w| w == latched)
            })),
            "the deck latched the installed release {release} from the pane's header; {}",
            claude.evidence()
        );
        claude.deck.wait_for_string("? for shortcuts");
        claude
    }

    /// The Claude Code process's environment, as the deck spawned it.
    pub fn environ(&self) -> Vec<(String, String)> {
        environ_of(self.pid)
    }

    pub fn env(&self, name: &str) -> Option<String> {
        self.environ()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The pane was installed: Claude Code's `VISUAL` and `EDITOR` name the
    /// deck's helper, and it has a generation and a tool environment file.
    /// Linux only, as the helper (`composer_probe::instance`) is.
    #[cfg(target_os = "linux")]
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
        assert!(
            self.env("CLAUDE_ENV_FILE")
                .is_some_and(|p| std::path::Path::new(&p).is_file()),
            "an installed Claude pane has its tool environment file"
        );
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

    /// The ops that reached the pane's writer since byte `since` of the log.
    pub fn ops(&self, since: usize) -> Vec<Op> {
        ops_in(self.log().get(since..).unwrap_or(""))
    }

    /// The doorbell tick's checks since `since`, each its outcome's text.
    pub fn checks(&self, since: usize) -> Vec<String> {
        checks_in(self.log().get(since..).unwrap_or(""))
    }

    /// The checks since `since` the daemon read the box non-empty or empty:
    /// `empty read applied` lines, and the reads the tick reports.
    pub fn empty_reads(&self, since: usize) -> usize {
        self.log()
            .get(since..)
            .unwrap_or("")
            .lines()
            .filter(|line| line.contains("empty read applied"))
            .count()
    }

    pub fn pane_frame(&self) -> (String, (u16, u16)) {
        let record = self.record();
        let raw = common::pane_snapshot_on(self.deck.attach_socket_path(), &self.pane.agent);
        let mut parser = dot_agent_deck::vt_grid::parser(
            record["rows"].as_u64().unwrap() as u16,
            record["cols"].as_u64().unwrap() as u16,
            0,
        );
        parser.process(&raw);
        (
            parser.screen().contents(),
            parser.screen().cursor_position(),
        )
    }

    pub fn pane_grid(&self) -> String {
        self.pane_frame().0
    }

    /// The input box's text, without the prompt glyph and hint: the last
    /// `❯` row drawn right under a rule, as Claude Code draws its box (a
    /// transcript's `❯` rows have none above them). `None` while the box
    /// isn't drawn, as while the deck's check or an editor runs.
    pub fn composer(&self) -> Option<String> {
        input_box(&self.pane_grid())
    }

    /// The composer row as drawn, and the cursor.
    pub fn composer_frame(&self) -> (Option<String>, (u16, u16)) {
        let (grid, cursor) = self.pane_frame();
        (
            grid.lines().nth(usize::from(cursor.0)).map(str::to_string),
            cursor,
        )
    }

    pub fn expect_composer(&self, expected: &str) {
        assert!(
            common::wait_until(Duration::from_secs(10), || self.composer().as_deref()
                == Some(expected)),
            "agent must draw exact composer {expected:?}, got {:?}; {}",
            self.composer(),
            self.evidence()
        );
    }

    pub fn evidence(&self) -> String {
        common::redact_credentials_for_output(&format!(
            "inbox={}\nrecord={}\nPane:\n{}\nGrid:\n{}\nLog (last 300 lines but the ring's per-tick `not admitted`):\n{}",
            self.listing(),
            self.record(),
            inbox::screen(&self.deck, &self.pane),
            self.deck.snapshot_grid(),
            tail(
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

    pub fn post(&self, body: &str) -> String {
        let reply = attach(
            &self.deck,
            json!({"op":"post-message","pane_id":self.pane.id,
            "expected_agent_id":self.pane.agent,"subject":SUBJECT,"body":body,"origin":"seed"}),
        );
        assert_eq!(reply["ok"], true, "post: {reply}");
        reply["id"].as_str().unwrap().into()
    }

    pub fn directive(&self, name: &str) -> String {
        format!(
            "Use the Bash tool to run exactly: printf '%s\\n' 'inbox-action-{name}' >> '{name}'. Then reply DONE-{name} and end your turn. Do this once for this message id."
        )
    }

    /// The task `id` was acted on once, acknowledged after one hand-over,
    /// and labelled as deck content, never user history.
    pub fn done(&self, id: &str, name: &str) {
        let path = self.deck.workdir().join(name);
        // The shell creates the file for `>>` before printf writes the line:
        // wait for a whole line, not only for the file.
        assert!(
            common::wait_until(BUDGET, || std::fs::read_to_string(&path)
                .is_ok_and(|text| text.ends_with('\n'))),
            "real Haiku must act on {id}; {}",
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
        assert!(
            !self.record()["live"]["last_user_prompt"]
                .as_str()
                .unwrap_or("")
                .contains(name),
            "deck content must not become user history; {}",
            self.evidence()
        );
    }

    /// Wait for the agent's turn to end and its next native waiter, then
    /// kill that waiter, so the next task can arrive only by the doorbell.
    pub fn idle_without_waiter(&self) {
        assert!(
            common::wait_until(BUDGET, || {
                self.listing()["armed"].as_u64().unwrap_or(0) > 0
                    && self.record()["live"]["status"] == "Idle"
            }),
            "the turn must finish; {}",
            self.evidence()
        );
        self.kill_waiter();
    }

    /// SIGKILL the pane's one armed native waiter, by the pid captured from
    /// `/proc` (its command and this deck's HOME).
    pub fn kill_waiter(&self) {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let env = std::fs::read(entry.path().join("environ")).unwrap_or_default();
            if cmd.split(|c| *c == 0).any(|a| a == b"wait")
                && cmd.split(|c| *c == 0).any(|a| a == b"inbox")
                && env
                    .split(|c| *c == 0)
                    .any(|v| v == format!("HOME={}", self.deck.home_dir().display()).as_bytes())
            {
                pids.push(pid);
            }
        }
        assert_eq!(pids.len(), 1, "one captured scratch waiter: {pids:?}");
        // SAFETY: a plain kill(2) of the pid captured above.
        assert_eq!(unsafe { libc::kill(pids[0], libc::SIGKILL) }, 0);
        assert!(common::wait_until(Duration::from_secs(10), || self
            .listing()["armed"]
            == 0));
    }

    /// A control task delivered by the doorbell into the untouched empty
    /// box, then the next waiter killed: the doorbell is proven to type into
    /// this pane, and the next task can arrive only by it.
    pub fn doorbell_ready(&self, label: &str) {
        self.kill_waiter();
        let control = format!("wake-control-{label}-0428.txt");
        let since = self.log().len();
        let id = self.post(&self.directive(&control));
        self.done(&id, &control);
        assert!(
            self.log()[since..].contains("doorbell rang"),
            "the control must come through the doorbell; {}",
            self.evidence()
        );
        self.idle_without_waiter();
        self.expect_composer("");
    }
}

/// What the person typed into agent `agent`'s pane in `text`, as the
/// daemon's `STREAM_IN` trace escapes it, concatenated in order.
pub fn human_in(text: &str, agent: &str) -> String {
    text.lines()
        .filter(|l| {
            l.contains("STREAM_IN forwarded to PTY writer")
                && l.contains(&format!("agent_id={agent} "))
        })
        .filter_map(|l| l.rsplit_once(" payload=").map(|(_, p)| p.to_string()))
        .collect()
}

/// The input box's text in a pane grid: the last `❯` row drawn right under
/// a rule, without the glyph and hint. `None` while the box isn't drawn.
pub fn input_box(grid: &str) -> Option<String> {
    let rows: Vec<&str> = grid.lines().collect();
    (1..rows.len()).rev().find_map(|i| {
        let tail = rows[i].trim_start().strip_prefix('\u{276f}')?;
        rows[i - 1].contains("\u{2500}\u{2500}").then(|| {
            tail.split("\u{b7} ? for shortcuts")
                .next()
                .unwrap()
                .trim()
                .to_string()
        })
    })
}

/// The doorbell tick's checks logged in `text`, each its outcome's text
/// (`Written` for a check the deck wrote), whatever pane they were for.
pub fn checks_in(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.contains("doorbell tick probe"))
        .map(|line| {
            line.split("outcome=")
                .nth(1)
                .unwrap_or("")
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// The preflight of a test that needs only the rollout, not a verified
/// installed release: real Claude Code can run here, and this build installs
/// `Direct(Claude)` panes. Otherwise the reason, for `SKIP:`.
pub fn rollout_installs_claude() -> Result<(), String> {
    use dot_agent_deck::composer_probe::install::{INSTALL_ROLLOUT, PaneClass, ProbeAgent};
    common::check_claude_available()?;
    if INSTALL_ROLLOUT.contains(&PaneClass::Direct(ProbeAgent::Claude)) {
        Ok(())
    } else {
        Err(format!(
            "this build installs no Direct(Claude) pane (INSTALL_ROLLOUT = {INSTALL_ROLLOUT:?})"
        ))
    }
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The ops logged in `text`, in order.
pub fn ops_in(text: &str) -> Vec<Op> {
    text.lines()
        .filter(|line| line.contains("an op moved the epoch"))
        .filter_map(|line| {
            let writer = line.split(" writer=").nth(1)?;
            let len = line
                .split(" len=")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()?;
            Some(Op {
                deck: writer.starts_with("Deck") || writer.starts_with("UnnamedDeck"),
                len,
            })
        })
        .collect()
}

/// `pid`'s environment.
pub fn environ_of(pid: u32) -> Vec<(String, String)> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// The Claude Code process of `agent` under `deck`: its argv starts with a
/// `claude` executable, and its environment carries this deck's HOME and the
/// agent's id. With it, the pane's hook capability from that environment.
fn claude_process(deck: &common::TuiDeck, agent: &str) -> Option<(u32, String)> {
    let home = format!("HOME={}", deck.home_dir().display());
    let id = format!("DOT_AGENT_DECK_AGENT_ID={agent}");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let cmd = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let argv0 = cmd.split(|c| *c == 0).next().unwrap_or_default();
        if !String::from_utf8_lossy(argv0).ends_with("claude") {
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
