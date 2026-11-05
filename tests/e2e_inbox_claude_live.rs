#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Interactive Claude inbox delivery, with a real Haiku agent in an attached PTY.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/claude_installed.rs"]
mod installed;

use inbox::{attach, hook, request};
use serde_json::{Value, json};
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

const SUBJECT: &str = "live inbox task";
const BUDGET: Duration = Duration::from_secs(120);

struct Claude {
    pane: inbox::Pane,
    deck: common::TuiDeck,
    /// The daemon's log file, in the deck's work directory.
    log_name: &'static str,
}

/// The Claude Code command every test here runs.
const CLAUDE: &str = "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write";

impl Claude {
    fn new() -> Self {
        Self::launch(&common::capability_export_command(&format!(
            "exec {CLAUDE}"
        )))
    }

    /// PRD #1666 part 3 (R-1): Claude Code in the pane, but outside the
    /// process tree the deck recorded for it. The pane's process records its
    /// pid and becomes `sleep`; Claude Code is started in a subshell that
    /// exits at once, so it is re-parented away from that process while it
    /// keeps the pane's terminal and environment — the shape of Claude Code's
    /// background session host, whose hooks and tool shell run outside the
    /// deck's agent. Claude Code records its own pid.
    fn detached() -> Self {
        Self::launch(&common::capability_export_command(&format!(
            "printf '%s' \"$$\" > root.pid; \
             ( exec {CLAUDE} < /dev/tty & printf '%s' \"$!\" > claude.pid ); \
             exec sleep 86400"
        )))
    }

    fn launch(command: &str) -> Self {
        Self::launch_with(command, &[])
    }

    /// [`Self::launch`] with `extra` set for the deck and its daemon.
    fn launch_with(command: &str, extra: &[(&str, &str)]) -> Self {
        let command = command.to_string();
        let mut builder = common::TuiDeck::builder();
        for (key, value) in extra {
            builder = builder.with_env(*key, *value);
        }
        let deck = builder
            .with_pty_size(180, 50)
            .with_imported_claude_credentials()
            .with_claude_trust_workdir()
            .with_env("DOT_AGENT_DECK_LOG", "inbox-live.log")
            .with_env(
                "RUST_LOG",
                "info,pane_write=trace,dot_agent_deck::doorbell=debug",
            )
            .with_env("DOT_AGENT_DECK_INBOX_ARM_GRACE_MS", "2000")
            .with_env("DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS", "30000")
            .with_env("DOT_AGENT_DECK_INBOX_WAKE_STALL_AFTER_MS", "2000")
            .with_continue_session("inbox-haiku", command)
            .launch_with_fixture("minimal");
        deck.wait_for_string("inbox-haiku");
        let record = std::cell::RefCell::new(Value::Null);
        assert!(
            common::wait_until(BUDGET, || {
                *record.borrow_mut() = attach(&deck, json!({"op":"list-agents"}))["agent_records"]
                    .as_array()
                    .and_then(|rs| rs.iter().find(|r| r["display_name"] == "inbox-haiku"))
                    .cloned()
                    .unwrap_or(Value::Null);
                record.borrow()["pane_id_env"].is_string()
            }),
            "Claude pane must register"
        );
        let record = record.into_inner();
        let agent = record["id"].as_str().unwrap().to_string();
        let token = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
            .unwrap();
        let pane = inbox::Pane {
            id: record["pane_id_env"].as_str().unwrap().into(),
            agent,
            token,
            pid: 0,
            capture: PathBuf::new(),
            attach_socket: deck.attach_socket_path().into(),
        };
        let result = Self {
            deck,
            pane,
            log_name: "inbox-live.log",
        };
        assert!(
            common::wait_until(BUDGET, || result.listing()["armed"].as_u64().unwrap_or(0)
                > 0),
            "native SessionStart waiter must arm; {}",
            result.evidence()
        );
        result.deck.wait_for_string("Claude Code v");
        result
    }

    fn listing(&self) -> Value {
        let mut req = request(&self.pane, "inbox_list");
        req["all"] = json!(false);
        hook(&self.deck, req)
    }

    fn record(&self) -> Value {
        attach(&self.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == self.pane.agent)
            .unwrap()
            .clone()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.deck.workdir().join(self.log_name)).unwrap_or_default()
    }

    /// PRD #1666 part 3 (Q35-28): interactive Haiku in a pane the deck
    /// installs its editor helper into (`support/claude_installed.rs`), so
    /// the production tick checks a touched input box. Run only where
    /// [`installed::checkable`] holds.
    fn installed() -> Self {
        let installed = installed::Installed::new(&[]);
        installed.assert_installed();
        Self {
            pane: installed.pane,
            deck: installed.deck,
            log_name: installed::LOG,
        }
    }

    /// The input box's text, `None` while it isn't drawn (as during the
    /// deck's check, when Claude Code hands the box to the helper).
    fn input_box(&self) -> Option<String> {
        installed::input_box(&self.pane_grid())
    }

    /// The ops that reached the pane's writer since byte `since` of the log.
    fn ops(&self, since: usize) -> Vec<installed::Op> {
        installed::ops_in(self.log().get(since..).unwrap_or(""))
    }

    /// The doorbell tick's checks since byte `since` of the log, each its
    /// outcome's text.
    fn checks(&self, since: usize) -> Vec<String> {
        installed::checks_in(self.log().get(since..).unwrap_or(""))
    }

    /// What the person typed into the pane since byte `since` of the log.
    fn human_in(&self, since: usize) -> String {
        installed::human_in(self.log().get(since..).unwrap_or(""), &self.pane.agent)
    }

    /// Read the agent's composer using its actual PTY geometry, rather than
    /// interpreting its bytes at the stand-in's fixed size.
    fn pane_grid(&self) -> String {
        self.pane_frame().0
    }

    fn pane_frame(&self) -> (String, (u16, u16)) {
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

    fn composer(&self) -> Option<String> {
        if self
            .pane_grid()
            .contains("Your conversation moved to the background")
        {
            return None;
        }
        self.pane_grid().lines().rev().find_map(|row| {
            let tail = row.trim_start().strip_prefix('❯')?;
            Some(
                tail.split("· ? for shortcuts")
                    .next()
                    .unwrap()
                    .trim()
                    .to_string(),
            )
        })
    }

    fn expect_composer(&self, expected: &str) {
        assert!(
            common::wait_until(Duration::from_secs(10), || self.composer().as_deref()
                == Some(expected)),
            "agent must draw exact composer {expected:?}, got {:?}; {}",
            self.composer(),
            self.evidence()
        );
    }

    fn forwarded(&self, since: usize, payload: &str) -> bool {
        self.log().get(since..).unwrap_or("").lines().any(|line| {
            line.contains("STREAM_IN forwarded to PTY writer")
                && line.contains(&format!("agent_id={}", self.pane.agent))
                && line.contains(&format!("payload={payload}"))
        })
    }

    fn evidence(&self) -> String {
        format!(
            "inbox={}\nrecord={}\nPane:\n{}\nGrid:\n{}\nLog:\n{}",
            self.listing(),
            self.record(),
            inbox::screen(&self.deck, &self.pane),
            self.deck.snapshot_grid(),
            self.log()
        )
    }

    fn post(&self, body: &str) -> String {
        let reply = attach(
            &self.deck,
            json!({"op":"post-message","pane_id":self.pane.id,
            "expected_agent_id":self.pane.agent,"subject":SUBJECT,"body":body,"origin":"seed"}),
        );
        assert_eq!(reply["ok"], true, "post: {reply}");
        reply["id"].as_str().unwrap().into()
    }

    fn directive(&self, name: &str) -> String {
        format!(
            "Use the Bash tool to run exactly: printf '%s\\n' 'inbox-action-{name}' >> '{name}'. Then reply DONE-{name} and end your turn. Do this once for this message id."
        )
    }

    fn done(&self, id: &str, name: &str, native: bool) {
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
            common::wait_until(BUDGET, || self.listing()["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == id && m["state"] == "acknowledged")),
            "message must be acknowledged; {}",
            self.evidence()
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("inbox-action-{name}\n"),
            "one append proves one action"
        );
        let message = self.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()
            .clone();
        assert_eq!(message["deliveries"], 1, "one hand-over: {message}");
        assert!(
            !self.record()["live"]["last_user_prompt"]
                .as_str()
                .unwrap_or("")
                .contains(name),
            "deck content must not become user history; {}",
            self.evidence()
        );
        assert!(
            !self.record()["live"]["first_prompts"]
                .to_string()
                .contains(name)
        );
        assert!(
            self.deck
                .wait_for_grid_string_within(&format!("✉ {SUBJECT}"), Duration::from_secs(15)),
            "card must label deck content; {}",
            self.evidence()
        );
        if native {
            assert!(
                !self.log().contains("doorbell rang"),
                "native wake typed a doorbell; {}",
                self.evidence()
            );
            assert!(
                !inbox::screen(&self.deck, &self.pane)
                    .contains("waiting, run: dot-agent-deck inbox fetch")
            );
        }
    }
}

/// Scenario: Leave genuine interactive Haiku idle in its attached deck pane and post a sentinel-writing inbox task without typing into the pane. Claude must visibly work, acknowledge it once, show the envelope card label and keep the task out of human prompt history without a doorbell.
#[spec("inbox/claude/011")]
#[test]
fn claude_011_live_idle_native_wake() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let name = "native-idle-6b4fe2.txt";
    let id = claude.post(&claude.directive(name));
    claude.deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &["Bash"],
        BUDGET,
    );
    claude.done(&id, name, true);
}

/// Scenario: Ask interactive Haiku to run a slow shell operation, then post a second inbox task while that operation is running. At turn end the native Stop waiter must hand over the second task exactly once without typing a doorbell.
#[spec("inbox/claude/012")]
#[test]
fn claude_012_live_busy_fold_at_stop() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let name = "native-busy-e296c1.txt";
    // PRD #1666: absolute paths, so the model need not infer its directory.
    let work = claude.deck.workdir();
    let first = claude.post(&format!(
        "Use Bash to run exactly: printf busy > '{}'; sleep 8; printf done > '{}'. Then reply BUSY-DONE and stop.",
        work.join("busy-started-781d.txt").display(),
        work.join("busy-finished-781d.txt").display()
    ));
    assert!(
        common::wait_until(BUDGET, || claude
            .deck
            .workdir()
            .join("busy-started-781d.txt")
            .exists()),
        "slow turn must start; {}",
        claude.evidence()
    );
    assert!(
        !claude
            .deck
            .workdir()
            .join("busy-finished-781d.txt")
            .exists()
    );
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, true);
    assert!(
        claude.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == first && m["state"] == "acknowledged")
    );
}

/// Scenario: Run /clear twice in interactive Haiku. Each clear's SessionStart arms a new waiter, which takes the place of the one before it, so exactly one stays armed. Then post one append task. Exactly one hand-over, acknowledgement and sentinel line must result, with no doorbell.
#[spec("inbox/claude/013")]
#[test]
fn claude_013_live_clear_waiters_claim_once() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let registered = || {
        claude
            .log()
            .lines()
            .filter(|line| {
                line.contains("receipt receiver registered") && line.contains("once_waiter")
            })
            .count()
    };
    assert!(
        common::wait_until(BUDGET, || registered() >= 1),
        "startup must arm a waiter; {}",
        claude.evidence()
    );
    for clear in ["first", "second"] {
        let seen = registered();
        claude.deck.send_bytes(b"/clear\r");
        assert!(
            common::wait_until(BUDGET, || registered() > seen),
            "the {clear} clear must arm another waiter; {}",
            claude.evidence()
        );
        assert!(
            common::wait_until(BUDGET, || claude.listing()["armed"] == 1),
            "the newer waiter takes the older one's place; {}",
            claude.evidence()
        );
    }
    let name = "native-clear-44f81c.txt";
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, true);
}

/// The installed Claude Code release, read from `claude --version`.
#[cfg(target_os = "linux")]
fn installed_claude_release() -> Result<String, String> {
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

/// Whether the deck types into Claude Code `release`: Claude Code's facts
/// admit every latched release, so this is false only when the facts are not
/// `Go`.
#[cfg(target_os = "linux")]
fn measured(release: &str) -> bool {
    use dot_agent_deck::doorbell_activation::{AGENT_FACTS, Activation, LatchedRelease};
    semver::Version::parse(release).is_ok_and(|version| {
        matches!(
            dot_agent_deck::doorbell_activation::activation(
                &AGENT_FACTS,
                &dot_agent_deck::event::AgentType::ClaudeCode,
                &LatchedRelease::Known(version),
            ),
            Activation::Go { .. }
        )
    })
}

/// `inbox/claude/014`'s preflight: it runs only when the deck types into the
/// installed release, and is otherwise UNVERIFIED, never green.
#[cfg(target_os = "linux")]
fn claude_release_measured() -> Result<(), String> {
    let installed = installed_claude_release()?;
    eprintln!("Claude Code release under test: {installed}");
    if measured(&installed) {
        Ok(())
    } else {
        Err(format!(
            "Claude Code {installed} is installed, and the deck's Claude Code facts are not Go: \
             the doorbell does not type into it"
        ))
    }
}

/// Scenario: On the installed Claude Code release, capture the pid of the scratch pane's armed waiter and kill that process, then post a task after the waiter disconnects. The token-led doorbell must wake interactive Haiku, whose submit hook injects and acknowledges the task and shows an envelope label before the sentinel is written; if the deck's Claude Code facts are not Go the test reports SKIP saying so.
#[spec("inbox/claude/014")]
#[test]
#[cfg(target_os = "linux")]
fn claude_014_live_killed_waiter_falls_back_to_injection() {
    skip_unless!(common::check_claude_available());
    skip_unless!(claude_release_measured());
    let claude = Claude::new();
    kill_trial_waiter(&claude);
    let name = "doorbell-fallback-908c2a.txt";
    let id = claude.post(&claude.directive(name));
    claude.done(&id, name, false);
    assert!(
        claude.log().contains("doorbell rang"),
        "fallback must ring; {}",
        claude.evidence()
    );
    assert!(
        !claude.record()["live"]["last_user_prompt"]
            .to_string()
            .contains("waiting, run:")
    );
}

/// Scenario: Run interactive Haiku in a deck started with the Claude Code doorbell turned off (`DOT_AGENT_DECK_DOORBELL_CLAUDE=0`), kill its scratch pane's armed waiter, and post a directive task naming a sentinel. The daemon's release line must name the installed release as latched and its `doorbell off` line must name turned_off; for 20 seconds after that no doorbell rings or shows on the pane's screen or in its last prompt, the task stays queued, and the card shows the cannot_type words. The test then types a short prompt into the pane and presses Enter: the prompt hook hands Haiku the task, Haiku writes the sentinel, and the task is acknowledged.
#[spec("inbox/claude/050")]
#[test]
#[cfg(target_os = "linux")]
fn claude_050_live_turned_off_doorbell_is_held_and_a_prompt_delivers() {
    skip_unless!(common::check_claude_available());
    let release = match installed_claude_release() {
        Ok(release) => release,
        Err(why) => {
            skip_unless!(Err(why));
            return;
        }
    };
    eprintln!("Claude Code release under test: {release}");
    let claude = Claude::launch_with(
        &common::capability_export_command(&format!("exec {CLAUDE}")),
        &[(dot_agent_deck::doorbell_switch::CLAUDE_ENV, "0")],
    );
    // The cause is proved, not assumed: the installed release latched, and
    // the doorbell held as turned_off.
    let latched = format!("release={release}");
    let saw_latch = common::wait_until(BUDGET, || {
        claude
            .log()
            .lines()
            .any(|line| line.contains("pane release") && line.contains(&latched))
    });
    if !saw_latch {
        let lines: Vec<String> = claude
            .log()
            .lines()
            .filter(|line| line.contains("pane release"))
            .map(str::to_string)
            .collect();
        skip_unless!(Err(format!(
            "the deck did not latch Claude Code {release}; its release lines: {lines:?}"
        )));
        return;
    }
    kill_trial_waiter(&claude);
    assert!(
        common::wait_until(BUDGET, || claude.listing()["armed"].as_u64() == Some(0)),
        "the waiter must go; {}",
        claude.evidence()
    );
    let name = "turned-off-doorbell-4d17b2.txt";
    let id = claude.post(&claude.directive(name));
    assert!(
        common::wait_until(BUDGET, || {
            claude
                .log()
                .lines()
                .any(|line| line.contains("doorbell off") && line.contains("turned_off"))
        }),
        "no `doorbell off` line named turned_off; {}",
        claude.evidence()
    );
    assert!(
        !common::wait_until(Duration::from_secs(20), || {
            claude.log().contains("doorbell rang") || claude.pane_grid().contains("waiting, run:")
        }),
        "a doorbell rang or its line is on the screen; {}",
        claude.evidence()
    );
    assert!(
        !claude.record()["live"]["last_user_prompt"]
            .to_string()
            .contains("waiting, run:")
    );
    assert!(
        claude.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id && m["state"] == "queued"),
        "the task stays queued; {}",
        claude.evidence()
    );
    assert!(
        claude.deck.wait_for_grid_string_within(
            dot_agent_deck::inbox_display::WAKE_HOLD_CANNOT_TYPE,
            BUDGET
        ),
        "the card shows the words; {}",
        claude.evidence()
    );
    claude
        .deck
        .send_keys(b"Check for new messages and do what they say.");
    claude.deck.send_keys(b"\r");
    claude.done(&id, name, false);
}

/// Scenario: Complete a fallback-delivered task in interactive Haiku, then post a second task and hold the deck's ring just after its line reaches the input box, before its Enter, while the user types into the pane. The deck submits its own line alone, Haiku acts on the task, and the user's text then sits in the input box exactly as typed, unsubmitted.
#[spec("inbox/doorbell/039")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_039_live_typing_while_line_lands() {
    skip_unless!(common::check_claude_available());
    skip_unless!(claude_release_measured());
    let claude = fallback_ready_with(
        Claude::launch_with(
            &common::capability_export_command(&format!("exec {CLAUDE}")),
            &[
                ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_PANE", "*"),
                ("DOT_AGENT_DECK_TEST_DOORBELL_PAUSE_DIR", "ring-barriers"),
            ],
        ),
        "typing",
    );
    let root = claude.deck.home_dir().join("ring-barriers");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("after-line.armed"), "").unwrap();
    let name = "typing-while-line-lands-39a1.txt";
    let id = claude.post(&claude.directive(name));
    assert!(
        common::wait_until(BUDGET, || root.join("after-line.reached").exists()),
        "the ring must reach its line; {}",
        claude.evidence()
    );
    let typed = "user-text-while-deck-types-39a1";
    claude.deck.send_keys(typed.as_bytes());
    assert!(
        common::wait_until(BUDGET, || std::fs::read_to_string(
            root.join("observations")
        )
        .unwrap_or_default()
        .contains("human-arrived")),
        "the user's keys must arrive while the deck holds the writer"
    );
    std::fs::write(root.join("after-line.release"), "").unwrap();
    claude.done(&id, name, false);
    assert!(
        claude.log().contains("doorbell rang"),
        "the deck's line was submitted; {}",
        claude.evidence()
    );
    claude.expect_composer(typed);
    assert!(
        !claude.record()["live"]["last_user_prompt"]
            .to_string()
            .contains(typed),
        "the user's text was not submitted; {}",
        claude.evidence()
    );
}

/// Scenario: In a pane the deck installs its editor helper into, complete a doorbell-delivered task in interactive Haiku, press Right in its empty input box, then visibly move across and erase a harmless marker before posting another task. A check must read the box empty and the doorbell deliver the next task, creating its sentinel without further human input; Ctrl+U is used only after a timeout to diagnose an empty box held as a draft.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_right_in_empty_editor_does_not_strand_task() {
    skip_unless!(installed::checkable());
    empty_editor_fallback("right", false);
}

/// Scenario: In a pane the deck installs its editor helper into, complete a doorbell-delivered task in interactive Haiku, type a unique draft, and erase every character with Backspace before posting another task. A check must read the box empty and the doorbell deliver the task without another keypress, while the genuine-draft cases keep protecting text that remains unsent.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_backspaced_empty_editor_does_not_strand_task() {
    skip_unless!(installed::checkable());
    empty_editor_fallback("backspaced", true);
}

#[cfg(target_os = "linux")]
fn fallback_ready(label: &str) -> Claude {
    fallback_ready_with(Claude::installed(), label)
}

#[cfg(target_os = "linux")]
fn fallback_ready_with(claude: Claude, label: &str) -> Claude {
    claude.deck.wait_for_string("? for shortcuts");
    kill_trial_waiter(&claude);
    let control = format!("wake-control-{label}-7e91.txt");
    let id = claude.post(&claude.directive(&control));
    claude.done(&id, &control, false);
    // A completed model action alone can precede its Stop hook. Wait for the
    // next native waiter and genuine Idle before disabling native delivery.
    assert!(
        common::wait_until(BUDGET, || {
            claude.listing()["armed"].as_u64().unwrap_or(0) > 0
                && claude.record()["live"]["status"] == "Idle"
        }),
        "control turn must finish; {}",
        claude.evidence()
    );
    kill_trial_waiter(&claude);
    claude.expect_composer("");
    claude
}

#[cfg(target_os = "linux")]
fn empty_editor_fallback(label: &str, erase_draft: bool) {
    let claude = fallback_ready(label);
    if erase_draft {
        let draft = "empty-editor-probe-7e91";
        claude.deck.send_keys(draft.as_bytes());
        claude.expect_composer(draft);
        claude.deck.send_keys(&vec![0x7f; draft.len()]);
        claude.expect_composer("");
    } else {
        claude.expect_composer("");
        let since = claude.log().len();
        claude.deck.send_keys(b"\x1b[C");
        assert!(
            common::wait_until(Duration::from_secs(10), || claude
                .forwarded(since, "\\x1b[C")),
            "Right must be forwarded before posting; {}",
            claude.evidence()
        );
        // A trace precedes the write. Instead, prove recipient processing:
        // the marker is drawn after the empty-editor Right in FIFO input
        // order, then Left/Right must visibly move across it. Backspace must
        // draw empty again. These editing keys leave the draft bit set.
        claude.deck.send_keys(b"r");
        claude.expect_composer("r");
        let end = claude.pane_frame().1;
        claude.deck.send_keys(b"\x1b[D");
        assert!(
            common::wait_until(Duration::from_secs(10), || {
                let (grid, cursor) = claude.pane_frame();
                grid.lines().any(|row| {
                    row.trim_start()
                        .strip_prefix('❯')
                        .is_some_and(|tail| tail.trim() == "r")
                }) && cursor == (end.0, end.1 - 1)
            }),
            "Left must visibly move before the marker (end={end:?}, current={:?}); {}",
            claude.pane_frame().1,
            claude.evidence()
        );
        claude.deck.send_keys(b"\x1b[C");
        assert!(
            common::wait_until(Duration::from_secs(10), || claude.pane_frame().1 == end),
            "Right must visibly move across the marker; {}",
            claude.evidence()
        );
        claude.deck.send_keys(b"\x7f");
        claude.expect_composer("");
        eprintln!(
            "Right processing barrier: marker drawn, Left/Right cursor transition confirmed at {end:?}, Backspace redrew exact empty composer"
        );
    }
    let name = format!("wake-empty-{label}-7e91.txt");
    let since = claude.log().len();
    let id = claude.post(&claude.directive(&name));
    let delivered = common::wait_until(Duration::from_secs(30), || {
        claude.deck.workdir().join(&name).exists()
    });
    let before_recovery = claude.evidence();
    let recovered = if delivered {
        false
    } else {
        // Diagnose the hold, and leave the real agent settled before failing.
        claude.deck.send_keys(b"\x15");
        common::wait_until(BUDGET, || claude.deck.workdir().join(&name).exists())
    };
    assert!(
        delivered,
        "empty editor stranded {label} task for 30s; Ctrl+U recovery={recovered}; before recovery:\n{before_recovery}\nafter recovery:\n{}",
        claude.evidence()
    );
    claude.done(&id, &name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "{label}: a check read the touched box empty, then the doorbell rang; {}",
        claude.evidence()
    );
}

/// Scenario: Complete a fallback task in interactive Haiku, then press Left in its empty composer to open the background-session overview and queue another task. For five seconds the input row must remain exactly empty with unchanged captured input bytes, the task queued and zero deliveries. This is a GREEN regression guard on the part-3 head because navigation sets the draft bit; after the fix the unrecognised overview must still hold delivery.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_left_opens_overview_and_the_line_is_not_typed_there() {
    skip_unless!(common::check_claude_available());
    // Its positive control is delivered by the doorbell, which types only
    // into a release the deck has measured.
    skip_unless!(claude_release_measured());
    // util-linux script records every input byte before relaying it to the
    // real interactive Claude PTY. No human keys follow Left, so any later
    // input is automatic, even if a partial token is erased between samples.
    let version = std::process::Command::new("script")
        .arg("--version")
        .output()
        .expect("Linux overview input ledger requires util-linux script on PATH");
    assert!(
        version.status.success(),
        "util-linux script preflight failed"
    );
    let claude = fallback_ready_with(
        Claude::launch(&common::capability_export_command(&format!(
            "exec script --quiet --flush --return --echo never --log-in overview-input.log --log-out /dev/null --command 'exec {CLAUDE}'"
        ))),
        "overview",
    );
    let input = || std::fs::read(claude.deck.workdir().join("overview-input.log")).unwrap();
    let before_left = input();
    claude.deck.send_keys(b"\x1b[D");
    assert!(
        common::wait_until(Duration::from_secs(10), || claude
            .pane_grid()
            .contains("Your conversation moved to the background")),
        "Left must open the overview; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || input()[before_left.len()..]
            .ends_with(b"\x1b[D")),
        "input ledger must capture the human Left before observation; {}",
        claude.evidence()
    );
    let input_before = input();
    let input_row = |grid: &str| {
        grid.lines()
            .rev()
            .find(|row| row.trim_start().starts_with('❯'))
            .map(str::to_string)
    };
    let empty_row = input_row(&claude.pane_grid()).expect("overview must draw its input row");
    assert_eq!(
        empty_row.trim_start().strip_prefix('❯').unwrap().trim(),
        "describe a task for a new session"
    );
    let since = claude.log().len();
    let name = "overview-held-7e91.txt";
    let id = claude.post(&claude.directive(name));
    let leaked = std::cell::Cell::new(false);
    // A bounded observation: the closure never asks to stop, so it samples
    // for the whole five seconds.
    common::wait_until(Duration::from_secs(5), || {
        let grid = claude.pane_grid();
        let listing = claude.listing();
        let message = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["id"] == id)
            .unwrap();
        let seen = grid.contains("dot-agent-deck: message")
            || input_row(&grid).as_deref() != Some(empty_row.as_str())
            || input() != input_before
            || claude.log()[since..].lines().any(|line| {
                line.contains("doorbell attempt")
                    || line.contains("doorbell held")
                    || line.contains("doorbell rang")
                    || line.contains("doorbell ring withdrawn")
                    || line.contains("doorbell ring failed")
                    || line.contains("doorbell write")
                    || line.contains("doorbell sent its cleanup CR")
                    || line.contains("pane_write:")
            })
            || !grid.contains("Your conversation moved to the background")
            || claude.deck.workdir().join(name).exists()
            || message["state"] != "queued"
            || message["deliveries"] != 0;
        leaked.set(leaked.get() || seen);
        false
    });
    let leaked = leaked.get();
    let listing = claude.listing();
    let message = listing["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["id"] == id)
        .unwrap();
    let evidence = claude.evidence();
    eprintln!(
        "overview observation: leaked={leaked}; input ledger unchanged={}; empty input row={empty_row:?}; message={message}; pane=\n{}",
        input() == input_before,
        claude.pane_grid()
    );
    assert!(
        !leaked
            && input() == input_before
            && message["state"] == "queued"
            && message["deliveries"] == 0,
        "deck must not type its line or act on Enter in the overview; {evidence}"
    );
}

/// Scenario: In a pane the deck installs its editor helper into, disable native delivery, leave a unique human draft in interactive Haiku's input box, and queue a sentinel task past the accelerated stall threshold. Repeat with a partly deleted draft: for both, a check reads the box non-empty, and for five seconds after it the draft is exact and unsent with zero deliveries, the deck has typed nothing but its check's byte, and the card shows the draft hold and a stall diagnostic. Repeat with a prompt recalled from history with Up: the deck refuses that screen, writes no check and types nothing, and the card says it cannot check the box. Each time the user then moves to the end and clears the whole draft; the next check reads the box empty and the doorbell delivers the original task once.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_kept_draft_stays_held() {
    skip_unless!(installed::checkable());
    let mut holds = Vec::new();
    for variant in ["typed", "history", "partial"] {
        let claude = Claude::installed();
        claude.deck.wait_for_string("? for shortcuts");
        kill_trial_waiter(&claude);
        let draft = format!("human-kept-{variant}-c67e91");
        if variant == "history" {
            let prompt =
                format!("{draft} Reply exactly HISTORY-READY and end your turn. Do not use tools.");
            claude.deck.send_keys(format!("{prompt}\r").as_bytes());
            assert!(
                common::wait_until(BUDGET, || {
                    claude.listing()["armed"].as_u64().unwrap_or(0) > 0
                        && claude.record()["live"]["status"] == "Idle"
                }),
                "history seed must finish; {}",
                claude.evidence()
            );
            kill_trial_waiter(&claude);
            claude.deck.send_keys(b"\x1b[A");
            claude.expect_composer(&prompt);
            holds.push((variant, recalled_draft_trial(&claude, &prompt)));
        } else {
            claude.deck.send_keys(draft.as_bytes());
            claude.expect_composer(&draft);
            if variant == "partial" {
                claude.deck.send_keys(b"-erase");
                claude.expect_composer(&format!("{draft}-erase"));
                claude.deck.send_keys(&[0x7f; 6]);
                claude.expect_composer(&draft);
            }
            // Navigation inside retained text is a touch like any other.
            claude.deck.send_keys(b"\x1b[D");
            claude.expect_composer(&draft);
            holds.push((variant, kept_draft_trial(&claude, variant, &draft)));
        }
    }
    for (variant, hold) in holds {
        // A recalled prompt can't be checked on the verified release: the
        // deck refuses its screen (see `recalled_draft_trial`).
        let expected = if variant == "history" {
            "cannot_check"
        } else {
            "possible_draft"
        };
        assert!(
            hold.to_string().contains(expected),
            "retained {variant} draft must report {expected}, got {hold}"
        );
    }
}

/// The history variant: a prompt recalled with Up into the box. On Claude
/// Code 2.1.317 the deck's admission refuses that screen (`Surface`) for the
/// first check and the retry, so no check is written: the task is held with
/// `cannot_check`, nothing is typed, and the draft stays exact. Clearing it
/// lets the next check read the box empty and the doorbell deliver once.
#[cfg(target_os = "linux")]
fn recalled_draft_trial(claude: &Claude, draft: &str) -> Value {
    let name = "kept-history-c67e91.txt";
    let since = claude.log().len();
    let id = claude.post(&claude.directive(name));
    assert!(
        common::wait_until(BUDGET, || claude.record()["inbox"]["wake_hold"]
            == "cannot_check"
            && claude.log()[since..].lines().any(|l| l
                .contains("probe refused when scheduled")
                && l.contains("refusal=Surface"))),
        "the recalled draft's screen is refused and the card says cannot_check; {}",
        claude.evidence()
    );
    eprintln!(
        "{}",
        common::redact_credentials_for_output(&format!(
            "recalled draft, screen refused: frame=\n{}",
            claude.pane_grid()
        ))
    );
    common::wait_until(Duration::from_secs(5), || {
        let message = claude.listing()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap();
        assert_eq!(message["state"], "queued", "{message}");
        assert_eq!(message["deliveries"], 0, "{message}");
        if let Some(text) = claude.input_box() {
            assert_eq!(text, draft, "the recalled draft changed");
        }
        assert!(
            claude.ops(since).iter().all(|o| !o.deck),
            "the deck typed nothing: {:?}",
            claude.ops(since)
        );
        assert_eq!(claude.human_in(since), "", "no input byte of the person's");
        assert!(
            !claude.checks(since).iter().any(|c| c == "Written")
                && !claude.log()[since..].contains("doorbell rang"),
            "no check written and no ring; {}",
            claude.evidence()
        );
        false
    });
    let hold = claude.record()["inbox"]["wake_hold"].clone();
    claude.deck.send_keys(b"\x05\x15");
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "clearing the recalled draft recovers through a check and the doorbell; {}",
        claude.evidence()
    );
    eprintln!(
        "retained history: screen refused (Surface), no check written, exact draft held 5s, zero deliveries, nothing typed; clear, an empty check and the doorbell delivered the original sentinel; wake_hold={hold}"
    );
    hold
}

/// The input box's row as drawn and the cursor, or `None` while the box
/// isn't drawn (the deck's check runs).
#[cfg(target_os = "linux")]
fn box_frame(claude: &Claude) -> Option<(Option<String>, (u16, u16))> {
    claude.input_box()?;
    let (grid, cursor) = claude.pane_frame();
    Some((
        grid.lines().nth(usize::from(cursor.0)).map(str::to_string),
        cursor,
    ))
}

/// PRD #1666 part 3 (Q35-21, Q35-28): task `id` (sentinel `name`), posted
/// at byte `since` of the log over a box the person touched, is held because
/// a check read the box non-empty. Waits for that check and for Claude Code
/// to draw its box again, then for five seconds: the task stays queued with
/// no delivery, any box drawn is the frame drawn after the check, the deck
/// writes nothing but one-byte checks, the person types nothing, and no
/// ring starts. The frame after the check.
#[cfg(target_os = "linux")]
fn checked_and_held(
    claude: &Claude,
    id: &str,
    name: &str,
    since: usize,
) -> (Option<String>, (u16, u16)) {
    assert!(
        common::wait_until(BUDGET, || claude.record()["inbox"]["wake_hold"]
            == "possible_draft"),
        "a check must read the box non-empty and hold the task; {}",
        claude.evidence()
    );
    // The tick logs its check once the probe returns, which can be after
    // the read it applied is on the card.
    assert!(
        common::wait_until(Duration::from_secs(30), || claude
            .checks(since)
            .iter()
            .any(|c| c == "Written")),
        "the hold comes from a check; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || box_frame(claude).is_some()),
        "Claude Code draws its box again after the check; {}",
        claude.evidence()
    );
    let held = box_frame(claude).unwrap();
    // A bounded observation: the closure asserts on every sample and never
    // asks to stop, so it samples for the whole five seconds.
    common::wait_until(Duration::from_secs(5), || {
        let listing = claude.listing();
        let message = listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["id"] == id)
            .unwrap();
        assert_eq!(message["state"], "queued", "{message}");
        assert_eq!(message["deliveries"], 0, "{message}");
        if let Some(frame) = box_frame(claude) {
            assert_eq!(frame, held, "the held box changed; {}", claude.evidence());
        }
        let ops = claude.ops(since);
        assert!(
            ops.iter().any(|o| o.deck) && ops.iter().filter(|o| o.deck).all(|o| o.len == 1),
            "the deck typed nothing but its checks' bytes: {ops:?}; {}",
            claude.evidence()
        );
        assert_eq!(
            claude.human_in(since),
            "",
            "no input byte of the person's after the post"
        );
        assert!(!claude.deck.workdir().join(name).exists());
        assert!(
            !claude.log()[since..].lines().any(|line| {
                line.contains("doorbell attempt") || line.contains("doorbell rang")
            }),
            "a non-empty box must prevent a ring; {}",
            claude.evidence()
        );
        false
    });
    held
}

/// After a hold over a whitespace-only draft whose row reads like the empty
/// box: `End` alone must put the cursor `columns` past `start`, so the draft
/// survived the check (a check is an editor round trip, and the row alone
/// can't tell blanks from an empty box). `End` is a touch: the next check
/// reads the box again and holds the task again.
#[cfg(target_os = "linux")]
fn end_reaches(claude: &Claude, start: (u16, u16), columns: u16, what: &str) {
    claude.deck.send_keys(b"\x05");
    let end = (start.0, start.1 + columns);
    assert!(
        common::wait_until(Duration::from_secs(30), || box_frame(claude)
            .is_some_and(|f| f.1 == end)),
        "{what}: End must reach the draft's end at {end:?}, so the draft survived the check; got {:?}; {}",
        box_frame(claude),
        claude.evidence()
    );
}

/// The installed release's banner, for the tests that name it.
#[cfg(target_os = "linux")]
fn installed_banner() -> String {
    format!(
        "Claude Code v{}",
        installed::installed_release().expect("the installed release")
    )
}

/// The input box's row is blank (`❯` and nothing else but blanks).
#[cfg(target_os = "linux")]
fn blank_row(frame: &(Option<String>, (u16, u16))) -> bool {
    frame.0.as_deref().is_some_and(|row| {
        row.trim_start()
            .strip_prefix('\u{276f}')
            .is_some_and(|tail| tail.trim().is_empty())
    })
}

/// Scenario: In a pane the deck installs its editor helper into, complete a doorbell-delivered control task in real interactive Claude Code of a release whose editor probe the deck has verified, disable its next native waiter, and type exactly three spaces before posting another sentinel task. A check reads the box non-empty; for five seconds after it the blank row and its three-column cursor offset stay exact, the person types nothing, the deck types nothing but its check's byte, the task stays queued with zero deliveries, and the card names the draft hold. Move to the end and clear the spaces; the next check reads the box empty and the original task is delivered once by the doorbell and writes its sentinel once.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_spaces_only_draft_stays_held() {
    skip_unless!(installed::checkable());
    let claude = fallback_ready_with(Claude::installed(), "spaces");
    assert!(
        claude.pane_grid().contains(&installed_banner()),
        "the verified release runs; {}",
        claude.evidence()
    );
    assert!(
        claude.log().contains("doorbell rang"),
        "positive control must really use the doorbell; {}",
        claude.evidence()
    );
    // Here the row's text reads the same with the three spaces as without
    // them. The cursor's exact three-column displacement, paired with the
    // unchanged blank row and the person's ledger, distinguishes three spaces
    // from the empty box.
    let empty = box_frame(&claude).expect("the box is drawn");
    assert!(
        blank_row(&empty),
        "control must leave a blank composer: {empty:?}; {}",
        claude.evidence()
    );
    let mark = claude.log().len();
    claude.deck.send_keys(b"   ");
    let spaces = (empty.0.clone(), (empty.1.0, empty.1.1 + 3));
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            box_frame(&claude) == Some(spaces.clone()) && claude.human_in(mark) == "   "
        }),
        "real Claude must retain exactly three spaces: empty={empty:?}, expected={spaces:?}, actual={:?}; {}",
        box_frame(&claude),
        claude.evidence()
    );
    let since = claude.log().len();
    let name = "spaces-held-4c29e1.txt";
    let id = claude.post(&claude.directive(name));
    let held = checked_and_held(&claude, &id, name, since);
    assert_eq!(
        held,
        spaces,
        "the check leaves the three spaces and the cursor as they were; {}",
        claude.evidence()
    );
    assert_eq!(
        claude.record()["inbox"]["wake_hold"],
        "possible_draft",
        "spaces must report the draft hold; {}",
        claude.evidence()
    );
    assert!(
        claude
            .deck
            .snapshot_grid()
            .contains("there may be an unsent draft"),
        "card must visibly explain the draft hold; {}",
        claude.evidence()
    );
    eprintln!(
        "{}",
        common::redact_credentials_for_output(&format!(
            "three-space hold: empty={empty:?}; spaces={spaces:?}; after the check={held:?}; 5s exact row/cursor, no person byte, only check bytes, queued with zero deliveries, no ring; wake_hold=possible_draft; card=\n{}",
            claude.deck.snapshot_grid()
        ))
    );
    // Clear from the end: Ctrl+U alone only erases up to the current cursor.
    claude.deck.send_keys(b"\x05\x15");
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "clearing spaces must recover through a check and the doorbell; {}",
        claude.evidence()
    );
    eprintln!(
        "three-space recovery: original {id} acknowledged, deliveries=1, sentinel appended once"
    );
}

/// Scenario: In a pane the deck installs its editor helper into, complete a doorbell-delivered control task in real interactive Claude Code of a release whose editor probe the deck has verified, disable its next native waiter, type exactly three spaces and then press Left three times, so the box's row and cursor are exactly those of the empty box again, before posting another sentinel task. A check reads the box non-empty, and Claude Code draws it back exactly as before the check; for five seconds after it the row and cursor stay those of the empty box, the person types nothing, the deck types nothing but its check's byte, the task stays queued with zero deliveries, and the card names the draft hold. End alone then puts the cursor three columns on, so the spaces survived the check. Clear them; the next check reads the box empty and the original task is delivered once by the doorbell and writes its sentinel once.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_spaces_moved_back_draft_stays_held() {
    skip_unless!(installed::checkable());
    let claude = fallback_ready_with(Claude::installed(), "spaces-back");
    assert!(
        claude.pane_grid().contains(&installed_banner()),
        "the verified release runs; {}",
        claude.evidence()
    );
    assert!(
        claude.log().contains("doorbell rang"),
        "positive control must really use the doorbell; {}",
        claude.evidence()
    );
    let empty = box_frame(&claude).expect("the box is drawn");
    assert!(
        blank_row(&empty),
        "control must leave a blank composer: {empty:?}; {}",
        claude.evidence()
    );
    let mark = claude.log().len();
    claude.deck.send_keys(b"   ");
    let spaces = (empty.0.clone(), (empty.1.0, empty.1.1 + 3));
    assert!(
        common::wait_until(Duration::from_secs(10), || box_frame(&claude)
            == Some(spaces.clone())),
        "real Claude must move its cursor past three spaces: expected={spaces:?}, actual={:?}; {}",
        box_frame(&claude),
        claude.evidence()
    );
    claude.deck.send_keys(b"\x1b[D\x1b[D\x1b[D");
    let typed = dot_agent_deck::pane_input::escape_bytes_for_log(b"   \x1b[D\x1b[D\x1b[D");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            box_frame(&claude) == Some(empty.clone()) && claude.human_in(mark) == typed
        }),
        "three spaces with the cursor moved back must draw the empty composer: empty={empty:?}, actual={:?}; {}",
        box_frame(&claude),
        claude.evidence()
    );
    let since = claude.log().len();
    let name = "spaces-back-held-8d31a7.txt";
    let id = claude.post(&claude.directive(name));
    let held = checked_and_held(&claude, &id, name, since);
    assert_eq!(
        held,
        empty,
        "after the check Claude Code draws the box as before it, the cursor back at the spaces' start; {}",
        claude.evidence()
    );
    assert_eq!(
        claude.record()["inbox"]["wake_hold"],
        "possible_draft",
        "spaces moved back must report the draft hold; {}",
        claude.evidence()
    );
    assert!(
        claude
            .deck
            .snapshot_grid()
            .contains("there may be an unsent draft"),
        "card must visibly explain the draft hold; {}",
        claude.evidence()
    );
    eprintln!(
        "{}",
        common::redact_credentials_for_output(&format!(
            "moved-back hold: empty={empty:?}; after the check={held:?}; 5s unchanged, no person byte, only check bytes, queued with zero deliveries, no ring; wake_hold=possible_draft; card=\n{}",
            claude.deck.snapshot_grid()
        ))
    );
    end_reaches(&claude, empty.1, 3, "the three spaces moved back");
    // Clear from the end: Ctrl+U alone erases only up to the cursor.
    claude.deck.send_keys(b"\x05\x15");
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "clearing the spaces must recover through a check and the doorbell; {}",
        claude.evidence()
    );
    eprintln!(
        "moved-back recovery: original {id} acknowledged, deliveries=1, sentinel appended once"
    );
}

/// Scenario: In a pane the deck installs its editor helper into, with real interactive Claude Code of a release whose editor probe the deck has verified and its native waiter armed, type two spaces, a non-ASCII character, Backspace and Ctrl+A, so the box is blank with the cursor at its start, then press Enter, which submits nothing; post a sentinel task that the native wake delivers, so the agent runs a whole turn of its own, then disable its next native waiter and post another. A check reads the box non-empty; for five seconds after it the blanks stay with the cursor where it was, the person types nothing, the deck types nothing but its check's byte, the task stays queued with zero deliveries and the card names the draft hold. End alone then puts the cursor two columns on, so the blanks survived the check. Clear them; the next check reads the box empty and the task is delivered once through the doorbell.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_blanks_enter_and_an_unrelated_turn_stay_held() {
    skip_unless!(installed::checkable());
    let claude = Claude::installed();
    claude.deck.wait_for_string("? for shortcuts");
    assert!(
        claude.pane_grid().contains(&installed_banner()),
        "the verified release runs; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(BUDGET, || claude.listing()["armed"].as_u64().unwrap_or(0)
            > 0),
        "the native waiter must be armed; {}",
        claude.evidence()
    );
    let mark = claude.log().len();
    let mut keys = b"  ".to_vec();
    keys.extend_from_slice("\u{e9}".as_bytes());
    claude.deck.send_keys(&keys);
    claude.expect_composer("\u{e9}");
    claude.deck.send_keys(b"\x7f");
    assert!(
        common::wait_until(Duration::from_secs(10), || box_frame(&claude)
            .is_some_and(|f| blank_row(&f))),
        "Backspace must leave a blank composer; {}",
        claude.evidence()
    );
    let after_spaces = box_frame(&claude).unwrap();
    claude.deck.send_keys(b"\x01");
    let at_start = (
        after_spaces.0.clone(),
        (after_spaces.1.0, after_spaces.1.1 - 2),
    );
    assert!(
        common::wait_until(Duration::from_secs(10), || box_frame(&claude)
            == Some(at_start.clone())),
        "Ctrl+A must move to the start of the two spaces: expected={at_start:?}, actual={:?}; {}",
        box_frame(&claude),
        claude.evidence()
    );
    claude.deck.send_keys(b"\r");
    let typed = dot_agent_deck::pane_input::escape_bytes_for_log(b"\x7f\x01\r");
    assert!(
        common::wait_until(Duration::from_secs(10), || claude
            .human_in(mark)
            .ends_with(&typed)),
        "the ledger must capture the Enter; {}",
        claude.evidence()
    );
    // A bounded observation: the closure asserts on every sample and never
    // asks to stop, so it samples for the whole two seconds.
    common::wait_until(Duration::from_secs(2), || {
        assert_eq!(
            box_frame(&claude),
            Some(at_start.clone()),
            "Enter on blanks submits nothing; {}",
            claude.evidence()
        );
        false
    });
    // A turn of the agent's own, started by its native wake, not by the
    // Enter.
    let turn = "blank-enter-native-3f81c2.txt";
    let turn_id = claude.post(&claude.directive(turn));
    claude.done(&turn_id, turn, true);
    assert!(
        common::wait_until(BUDGET, || {
            claude.listing()["armed"].as_u64().unwrap_or(0) > 0
                && claude.record()["live"]["status"] == "Idle"
        }),
        "the native turn must finish; {}",
        claude.evidence()
    );
    kill_trial_waiter(&claude);
    let before = box_frame(&claude).expect("the box is drawn");
    assert!(
        blank_row(&before),
        "the blanks must still be in a blank-looking composer: {before:?}; {}",
        claude.evidence()
    );
    let since = claude.log().len();
    let name = "blank-enter-held-3f81c2.txt";
    let id = claude.post(&claude.directive(name));
    let held = checked_and_held(&claude, &id, name, since);
    assert!(
        blank_row(&held),
        "the blanks stay blank after the check: before={before:?}, after={held:?}; {}",
        claude.evidence()
    );
    assert_eq!(
        held.1,
        before.1,
        "the check leaves the cursor at the blanks' start; {}",
        claude.evidence()
    );
    assert_eq!(
        claude.record()["inbox"]["wake_hold"],
        "possible_draft",
        "the blanks must report the draft hold; {}",
        claude.evidence()
    );
    assert!(
        claude
            .deck
            .snapshot_grid()
            .contains("there may be an unsent draft"),
        "card must visibly explain the draft hold; {}",
        claude.evidence()
    );
    eprintln!(
        "{}",
        common::redact_credentials_for_output(&format!(
            "blank-Enter hold after a native turn: at_start={at_start:?}; before the check={before:?}; after={held:?}; 5s unchanged, no person byte, only check bytes, queued with zero deliveries, no ring; wake_hold=possible_draft; card=\n{}",
            claude.deck.snapshot_grid()
        ))
    );
    end_reaches(&claude, before.1, 2, "the two blanks after Enter");
    claude.deck.send_keys(b"\x05\x15");
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "clearing the blanks must recover through a check and the doorbell; {}",
        claude.evidence()
    );
}

/// Scenario: In a pane the deck installs its editor helper into, with real interactive Claude Code of a release whose editor probe the deck has verified, complete a doorbell-delivered control task with native delivery disabled, then type the reviewer's sequence one key at a time: ab, Ctrl+J, two spaces, Home, Ctrl+U, Home, Backspace, Delete, Delete, Ctrl+U; Claude joins the second line to the first at that Ctrl+U, so two spaces stay in a box that looks empty. Post a sentinel task: a check reads the box non-empty, and for five seconds after it the box is unchanged, the cursor where it was, the person types nothing, the deck types nothing but its check's byte, the task stays queued with zero deliveries, no ring starts, and the card names the draft hold; End alone then puts the cursor two columns on, so the spaces survived the check; then type x and press Ctrl+C once, the remedy the docs name; the next check reads the box empty and the task is delivered once through the doorbell.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_reviewer_sequence_stays_held_until_a_letter_and_ctrl_c() {
    skip_unless!(installed::checkable());
    let claude = Claude::installed();
    claude.deck.wait_for_string("? for shortcuts");
    assert!(
        claude.pane_grid().contains(&installed_banner()),
        "the verified release runs; {}",
        claude.evidence()
    );
    let claude = fallback_ready_with(claude, "reviewer-seq");
    // One key at a time, as a user types them (the bytes the tester
    // measured: 61620a20201b5b48151b5b487f1b5b337e1b5b337e15).
    let keys: [&[u8]; 10] = [
        b"ab", b"\n", b"  ", b"\x1b[H", b"\x15", b"\x1b[H", b"\x7f", b"\x1b[3~", b"\x1b[3~",
        b"\x15",
    ];
    // Each key reaches the agent's PTY writer (the person's ledger) and the
    // agent has finished drawing for it (the frame stays the same over
    // several samples) before the next is sent, so no two arrive in one read.
    let mark = claude.log().len();
    let mut sent = Vec::new();
    for key in keys {
        claude.deck.send_keys(key);
        sent.extend_from_slice(key);
        let typed = dot_agent_deck::pane_input::escape_bytes_for_log(&sent);
        assert!(
            common::wait_until(Duration::from_secs(10), || claude.human_in(mark) == typed),
            "the ledger must capture {key:?}; {}",
            claude.evidence()
        );
        let seen = std::cell::RefCell::new((None, 0));
        assert!(
            common::wait_until(Duration::from_secs(10), || {
                let frame = claude.pane_frame();
                let mut seen = seen.borrow_mut();
                if seen.0.as_ref() == Some(&frame) {
                    seen.1 += 1;
                } else {
                    *seen = (Some(frame), 0);
                }
                seen.1 >= 5
            }),
            "the agent must settle after {key:?}; {}",
            claude.evidence()
        );
    }
    assert!(
        common::wait_until(Duration::from_secs(10), || box_frame(&claude)
            .is_some_and(|f| blank_row(&f))),
        "the two spaces must leave a blank-looking composer; {}",
        claude.evidence()
    );
    let before = box_frame(&claude).unwrap();
    let since = claude.log().len();
    let name = "reviewer-seq-held-4b1e07.txt";
    let id = claude.post(&claude.directive(name));
    let held = checked_and_held(&claude, &id, name, since);
    assert!(
        blank_row(&held),
        "the two spaces stay blank-looking after the check: before={before:?}, after={held:?}; {}",
        claude.evidence()
    );
    assert_eq!(
        held.1,
        before.1,
        "the check leaves the cursor at the two spaces' start; {}",
        claude.evidence()
    );
    assert_eq!(
        claude.record()["inbox"]["wake_hold"],
        "possible_draft",
        "the two spaces must report the draft hold; {}",
        claude.evidence()
    );
    assert!(
        claude
            .deck
            .snapshot_grid()
            .contains("there may be an unsent draft"),
        "card must visibly explain the draft hold; {}",
        claude.evidence()
    );
    eprintln!(
        "{}",
        common::redact_credentials_for_output(&format!(
            "reviewer sequence held: before the check={before:?}; after={held:?}; 5s unchanged, no person byte, only check bytes, queued with zero deliveries, no ring; wake_hold=possible_draft; card=\n{}",
            claude.deck.snapshot_grid()
        ))
    );
    end_reaches(&claude, before.1, 2, "the reviewer sequence's two spaces");
    // The remedy the docs name: a letter, so Ctrl+C has something to clear,
    // then Ctrl+C once.
    let remedy = claude.log().len();
    claude.deck.send_keys(b"x");
    claude.expect_composer("x");
    claude.deck.send_keys(b"\x03");
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "the remedy must recover through a check and the doorbell; {}",
        claude.evidence()
    );
    assert!(
        claude.human_in(remedy).starts_with("x\\x03"),
        "the remedy reached the pane before anything else: {:?}",
        claude.human_in(remedy)
    );
}

/// Where a person's Ctrl+C landed against the first check in `log`
/// (Q147b): accepted after that check's byte was written and before its
/// segment ended, i.e. while the check was running.
#[cfg(target_os = "linux")]
fn ctrl_c_inside_check(log: &str, agent: &str) -> bool {
    let lines: Vec<&str> = log.lines().collect();
    let written = lines
        .iter()
        .position(|l| l.contains("doorbell tick probe") && l.contains("outcome=Written"));
    let ctrl_c = lines.iter().position(|l| {
        l.contains("STREAM_IN forwarded to PTY writer")
            && l.contains(&format!("agent_id={agent} "))
            && l.ends_with("payload=\\x03")
    });
    let (Some(written), Some(ctrl_c)) = (written, ctrl_c) else {
        return false;
    };
    let ended = lines
        .iter()
        .enumerate()
        .skip(written)
        .find(|(_, l)| l.contains("segment ended") && l.contains("kind: Probe"))
        .map(|(i, _)| i);
    ctrl_c > written && ended.is_some_and(|ended| ctrl_c < ended)
}

/// The milliseconds from the first check's settled decision in `log` (where
/// a Claude release went before Q147b) to Claude's `?2004h` after it (E1,
/// where a held line-discipline key goes since Q151b: the hold it adds to
/// such a key), and to its segment's end (where Q147b sent it, which also
/// waits for the helper's exit drain).
#[cfg(target_os = "linux")]
fn decision_to_return_ms(log: &str) -> Option<(Option<f64>, f64)> {
    let at = |line: &str| -> Option<f64> {
        // `2026-11-04T07:01:46.103516Z ...`: seconds of the day.
        let time = line.get(11..26)?;
        let (h, rest) = time.split_once(':')?;
        let (m, sec) = rest.split_once(':')?;
        Some(
            h.parse::<f64>().ok()? * 3600.0
                + m.parse::<f64>().ok()? * 60.0
                + sec.parse::<f64>().ok()?,
        )
    };
    let mut lines = log
        .lines()
        .skip_while(|l| !(l.contains("doorbell tick probe") && l.contains("outcome=Written")));
    let settled = at(lines.find(|l| l.contains("probe settled"))?)?;
    let after: Vec<&str> = lines.collect();
    let ended = after
        .iter()
        .find(|l| l.contains("segment ended") && l.contains("kind: Probe"))?;
    let enabled = after
        .iter()
        .find(|l| l.contains("return markers") && l.contains("PasteOn"))
        .and_then(|l| at(l));
    Some((
        enabled.map(|e| (e - settled) * 1000.0),
        (at(ended)? - settled) * 1000.0,
    ))
}

/// Scenario: In a pane the deck installs its editor helper into, with real interactive Claude Code of a release whose editor probe the deck has verified, and native delivery disabled, type a letter and post a sentinel task, which a check holds behind that draft; then type a second letter, which starts another check, and press Ctrl+C the moment that check's byte is written, while Claude Code has handed its terminal to the check. Repeated, four trials to a freshly started deck, until twenty Ctrl+C presses have landed inside a check: every time Claude Code keeps running, the Ctrl+C clears the box, and the task is delivered once through the doorbell (Q147b: a Ctrl+C typed during a check had ended the session).
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_ctrl_c_during_a_check_leaves_claude_running() {
    skip_unless!(installed::checkable());
    const LANDED: usize = 20;
    const MAX_TRIALS: usize = 32;
    // A test deck's daemon stops itself after 300 s (the harness's lifetime
    // cap), and a trial is a Haiku turn of about 40 s: four to a deck.
    const PER_DECK: usize = 4;
    let mut landed = 0;
    let mut trials = 0;
    let mut decks = 0;
    while landed < LANDED && trials < MAX_TRIALS {
        decks += 1;
        let claude = Claude::installed();
        claude.deck.wait_for_string("? for shortcuts");
        assert!(
            claude.pane_grid().contains(&installed_banner()),
            "the verified release runs; {}",
            claude.evidence()
        );
        let claude = fallback_ready_with(claude, &format!("ctrl-c-check-{decks}"));
        for _ in 0..PER_DECK {
            if landed >= LANDED || trials >= MAX_TRIALS {
                break;
            }
            trials += 1;
            if ctrl_c_trial(&claude, trials) {
                landed += 1;
            }
            eprintln!("deck {decks}, trial {trials}: Ctrl+C inside the check: {landed} so far");
        }
    }
    assert!(
        landed >= LANDED,
        "only {landed} of {trials} Ctrl+C presses landed inside a check"
    );
}

/// One trial of `doorbell_010_live_ctrl_c_during_a_check_leaves_claude_running`
/// on an idle `claude` with native delivery disabled and an empty box:
/// whether its Ctrl+C landed inside a check. Claude Code must keep running,
/// the task must be delivered once, and the next waiter is killed again.
#[cfg(target_os = "linux")]
fn ctrl_c_trial(claude: &Claude, trial: usize) -> bool {
    let start = claude.log().len();
    claude.deck.send_keys(b"w");
    claude.expect_composer("w");
    let name = format!("ctrl-c-check-{trial}-5d2a.txt");
    let id = claude.post(&claude.directive(&name));
    assert!(
        common::wait_until(BUDGET, || claude.record()["inbox"]["wake_hold"]
            == "possible_draft"
            && claude.checks(start).iter().any(|c| c == "Written")),
        "trial {trial}: a check must hold the task behind the draft; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(Duration::from_secs(30), || claude
            .log()
            .get(start..)
            .unwrap_or("")
            .contains("segment ended")
            && claude.composer().as_deref() == Some("w")),
        "trial {trial}: the first check ends with the draft kept; {}",
        claude.evidence()
    );
    // The second letter touches the box, so the next tick checks it: the
    // Ctrl+C goes as soon as that check's byte is in the log. Polled with no
    // pause, since a check runs for tens of milliseconds, under
    // `wait_until`'s 50 ms step; it spins only until the tick's check.
    let touch = claude.log().len();
    claude.deck.send_keys(b"x");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !claude.checks(touch).iter().any(|c| c == "Written") {
        assert!(
            std::time::Instant::now() < deadline,
            "trial {trial}: the touch must start a check; {}",
            claude.evidence()
        );
        std::thread::yield_now();
    }
    claude.deck.send_keys(b"\x03");
    claude.done(&id, &name, false);
    let log = claude.log();
    let since = log.get(touch..).unwrap_or("");
    assert!(
        !since.contains("pane_command_exited") && !since.contains("event_type=SessionEnd"),
        "trial {trial}: Claude Code must keep running after the Ctrl+C; {}",
        claude.evidence()
    );
    assert!(
        claude.human_in(touch).starts_with("x\\x03"),
        "trial {trial}: the letter and the Ctrl+C reached the pane in order: {:?}",
        claude.human_in(touch)
    );
    let landed = ctrl_c_inside_check(since, &claude.pane.agent);
    eprintln!(
        "trial {trial}: Claude kept running; inside the check: {landed}; decision to Claude's return, to the segment's end: {:?} ms",
        decision_to_return_ms(since)
    );
    // As the control turn: the next native waiter, killed, so the next
    // trial's task is held and delivered by the doorbell.
    assert!(
        common::wait_until(BUDGET, || {
            claude.listing()["armed"].as_u64().unwrap_or(0) > 0
                && claude.record()["live"]["status"] == "Idle"
        }),
        "trial {trial}: the task's turn must finish; {}",
        claude.evidence()
    );
    kill_trial_waiter(claude);
    claude.expect_composer("");
    landed
}

/// Scenario: In a pane the deck installs its editor helper into, complete a doorbell-delivered control task in real interactive Haiku, send a prompt of your own, then recall it with Up and press Enter, which submits it again, while native delivery is disabled. Once that turn ends, post another sentinel task: a check must read the box empty and the doorbell type the task without another keypress.
#[spec("inbox/doorbell/010")]
#[test]
#[cfg(target_os = "linux")]
fn doorbell_010_live_history_enter_recovers_the_doorbell() {
    skip_unless!(installed::checkable());
    let claude = fallback_ready("history-enter");
    let idle_again = |what: &str| {
        assert!(
            common::wait_until(BUDGET, || {
                claude.listing()["armed"].as_u64().unwrap_or(0) > 0
                    && claude.record()["live"]["status"] == "Idle"
            }),
            "{what} must finish; {}",
            claude.evidence()
        );
        kill_trial_waiter(&claude);
    };
    let prompt =
        "history-enter-5a17d0 Reply exactly HISTORY-READY and end your turn. Do not use tools.";
    claude.deck.send_keys(format!("{prompt}\r").as_bytes());
    idle_again("the seeded prompt");
    claude.deck.send_keys(b"\x1b[A");
    claude.expect_composer(prompt);
    claude.deck.send_keys(b"\r");
    claude.expect_composer("");
    idle_again("the recalled prompt");
    let since = claude.log().len();
    let name = "history-enter-rung-5a17d0.txt";
    let id = claude.post(&claude.directive(name));
    let delivered = common::wait_until(Duration::from_secs(30), || {
        claude.deck.workdir().join(name).exists()
    });
    let before_recovery = claude.evidence();
    let recovered = if delivered {
        false
    } else {
        claude.deck.send_keys(b"\x05\x15");
        common::wait_until(BUDGET, || claude.deck.workdir().join(name).exists())
    };
    assert!(
        delivered,
        "the recalled prompt's Enter stranded the task for 30s; End/Ctrl+U recovery={recovered}; before recovery:\n{before_recovery}\nafter recovery:\n{}",
        claude.evidence()
    );
    claude.done(&id, name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "the task must come through a check that read the box empty and the doorbell; {}",
        claude.evidence()
    );
}

#[cfg(target_os = "linux")]
fn kept_draft_trial(claude: &Claude, variant: &str, draft: &str) -> Value {
    let name = format!("kept-{variant}-c67e91.txt");
    // A key that doesn't change the drawn box (Left in the typed and partial
    // variants) gives no drawing to wait for: wait for the person's ledger
    // to go quiet instead, so every key of theirs is before the post.
    let quiet = std::cell::Cell::new((claude.human_in(0).len(), std::time::Instant::now()));
    assert!(common::wait_until(Duration::from_secs(10), || {
        let n = claude.human_in(0).len();
        if n != quiet.get().0 {
            quiet.set((n, std::time::Instant::now()));
        }
        quiet.get().1.elapsed() >= Duration::from_millis(500)
    }));
    let since = claude.log().len();
    let id = claude.post(&claude.directive(&name));
    checked_and_held(claude, &id, &name, since);
    assert_eq!(
        claude.input_box().as_deref(),
        Some(draft),
        "retained {variant} draft is exact after the check; {}",
        claude.evidence()
    );
    assert!(
        common::wait_until(
            Duration::from_secs(10),
            || claude.record()["inbox"]["wake_stalled"] == true
        ),
        "{}",
        claude.evidence()
    );
    assert!(
        claude.log()[since..].lines().any(|line| {
            line.contains("inbox: wake stalled")
                && line.contains(&id)
                && line.contains("stalled=true")
        }),
        "matching stall log required; {}",
        claude.evidence()
    );
    let hold = claude.record()["inbox"]["wake_hold"].clone();
    // Ctrl+U deletes only to the cursor. The typed and partial variants
    // moved Left, so move to the end before clearing the whole draft.
    claude.deck.send_keys(b"\x05\x15");
    claude.done(&id, &name, false);
    assert!(
        claude.log()[since..].contains("empty read applied")
            && claude.log()[since..].contains("doorbell rang"),
        "clearing the {variant} draft recovers through a check and the doorbell; {}",
        claude.evidence()
    );
    eprintln!(
        "retained {variant}: checked non-empty, exact draft held 5s, zero deliveries, only check bytes, wake_stalled and matching log; clear, an empty check and the doorbell delivered the original sentinel; wake_hold={hold}"
    );
    hold
}

#[cfg(target_os = "linux")]
fn kill_trial_waiter(claude: &Claude) {
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
                .any(|v| v == format!("HOME={}", claude.deck.home_dir().display()).as_bytes())
        {
            pids.push(pid);
        }
    }
    assert_eq!(pids.len(), 1, "one captured scratch waiter: {pids:?}");
    assert_eq!(unsafe { libc::kill(pids[0], libc::SIGKILL) }, 0);
    assert!(common::wait_until(Duration::from_secs(10), || claude
        .listing()["armed"]
        == 0));
}

/// The pids from `pid`'s parent up, read with `ps`, at most 64 of them.
fn ancestors_of(pid: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    while chain.len() < 64 {
        let out = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &current.to_string()])
            .output()
            .expect("ps");
        let Ok(parent) = String::from_utf8_lossy(&out.stdout).trim().parse::<u32>() else {
            break;
        };
        chain.push(parent);
        if parent <= 1 {
            break;
        }
        current = parent;
    }
    chain
}

/// Kills the detached Claude Code by its own recorded pid when the test ends,
/// whatever the deck's teardown did.
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: a plain `kill(2)` on the pid this test's Claude recorded.
        unsafe {
            libc::kill(self.0 as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Scenario: Start genuine interactive Haiku in a deck pane through a launcher that lets go of it, so Claude Code runs outside the process tree the deck recorded for the pane while it keeps the pane's terminal and environment, as Claude Code's background session host does. Leave it idle and post a sentinel-writing inbox task without typing into the pane. Its hooks and its tool shell, all outside that tree, must still take the task: Claude visibly works, writes the sentinel once, the message is acknowledged after one hand-over and the card labels it.
#[spec("inbox/claude/046")]
#[test]
fn claude_046_live_outside_the_recorded_tree_takes_and_acknowledges() {
    skip_unless!(common::check_claude_available());
    let claude = Claude::detached();
    let read = |name: &str| {
        std::fs::read_to_string(claude.deck.workdir().join(name))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    };
    let (root, agent) = (read("root.pid").unwrap(), read("claude.pid").unwrap());
    let _kill = KillOnDrop(agent);
    let chain = ancestors_of(agent);
    assert!(
        !chain.contains(&root),
        "Claude Code {agent} must run outside the pane's recorded process {root}; its ancestors: \
         {chain:?}"
    );
    let name = "detached-claude-5d7a19.txt";
    let id = claude.post(&claude.directive(name));
    claude.deck.wait_for_strings_in_order_then_any_within(
        &["Thinking", "Working"],
        &["Bash"],
        BUDGET,
    );
    claude.done(&id, name, true);
}

/// PRD #1666 part 3 (R-3): start another interactive Haiku in `deck`, as
/// `name`, and return its pane once the deck has recorded it.
fn start_claude(deck: &common::TuiDeck, name: &str) -> inbox::Pane {
    let reply = attach(
        deck,
        json!({"op":"start-agent","command":common::capability_export_command(&format!("exec {CLAUDE}")),
               "cwd":deck.workdir(),"rows":40,"cols":140,"display_name":name,"agent_type":"claude_code",
               "env":[["DOT_AGENT_DECK_PANE_ID",name]]}),
    );
    assert_eq!(reply["ok"], true, "start {name}: {reply}");
    let agent = reply["id"].as_str().unwrap().to_string();
    let record = std::cell::RefCell::new(Value::Null);
    assert!(
        common::wait_until(BUDGET, || {
            *record.borrow_mut() = attach(deck, json!({"op":"list-agents"}))["agent_records"]
                .as_array()
                .and_then(|rs| rs.iter().find(|r| r["id"] == agent.as_str()))
                .cloned()
                .unwrap_or(Value::Null);
            record.borrow()["pane_id_env"].is_string()
        }),
        "{name} must register"
    );
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
        .unwrap();
    inbox::Pane {
        id: record.borrow()["pane_id_env"].as_str().unwrap().into(),
        agent,
        token,
        pid: 0,
        capture: PathBuf::new(),
        attach_socket: deck.attach_socket_path().into(),
    }
}

/// Scenario: With genuine interactive Haiku running in the deck, Claude's settings are made a pre-0.47 install whose status hooks run another dot-agent-deck and that has no waiter, and a second Haiku is started under them: its card names its hooks as too old for its messages. That copy is then gone, as after an upgrade, and the deck's installed copy runs `hooks refresh`, which moves the hooks onto it and adds the waiter. A Haiku started after that is woken natively by a sentinel-writing task, does it once, and the task is acknowledged.
#[spec("inbox/claude/049")]
#[test]
fn claude_049_live_hooks_refreshed_after_an_upgrade_deliver() {
    use std::os::unix::fs::PermissionsExt;
    skip_unless!(common::check_claude_available());
    let claude = Claude::new();
    let home = claude.deck.home_dir().to_path_buf();
    let installed = home.join(".local/bin/dot-agent-deck");
    assert!(
        installed.exists(),
        "the harness seeds the deck's durable copy"
    );

    // A pre-0.47 install: the status hooks run another copy, with no waiter.
    let old = home.join("old-deck/dot-agent-deck");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(
        &old,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'dot-agent-deck (older)'; exit 0; }}\nDOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION=pre-inbox exec '{}' \"$@\"\n",
            common::contained_deck_binary().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings_path = home.join(".claude/settings.json");
    let mut settings: Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    let ours = installed.to_string_lossy().into_owned();
    for (_, rules) in settings["hooks"].as_object_mut().unwrap().iter_mut() {
        for rule in rules.as_array_mut().unwrap() {
            let handlers = rule["hooks"].as_array_mut().unwrap();
            handlers.retain(|h| !h["command"].as_str().unwrap_or("").contains(" inbox wait "));
            for handler in handlers.iter_mut() {
                if let Some(command) = handler["command"].as_str()
                    && command.contains(&ours)
                {
                    handler["command"] = json!(command.replace(&ours, &old.to_string_lossy()));
                }
            }
        }
    }
    std::fs::write(&settings_path, settings.to_string()).unwrap();

    let stale = start_claude(&claude.deck, "stale-haiku");
    let notice = || {
        attach(&claude.deck, json!({"op":"list-agents"}))["agent_records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == stale.agent.as_str())
            .map(|r| r["inbox"]["integration"].clone())
            .unwrap_or(Value::Null)
    };
    assert!(
        common::wait_until(BUDGET, || notice() == "hooks_too_old"),
        "the stale agent's card names its hooks; {}",
        claude.evidence()
    );
    assert!(
        claude.deck.wait_for_grid_string_within(
            "deck hooks run a dot-agent-deck too old",
            Duration::from_secs(30)
        ),
        "the notice is on screen; {}",
        claude.evidence()
    );

    // The upgrade moved the older copy away; the installed copy refreshes.
    std::fs::remove_file(&old).unwrap();
    let out = std::process::Command::new(&installed)
        .args(["hooks", "refresh", "--json"])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(claude.deck.workdir())
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|error| panic!("a JSON report: {error}; {out:?}"));
    assert!(out.status.success() && report["ok"] == true, "{report}");
    let refreshed = std::fs::read_to_string(&settings_path).unwrap();
    assert!(
        refreshed.contains(" inbox wait ") && !refreshed.contains("old-deck"),
        "{refreshed}"
    );

    // Restarted on the refreshed hooks, the agent takes its next task.
    let pane = start_claude(&claude.deck, "fresh-haiku");
    let fresh = Claude {
        pane,
        deck: claude.deck,
        log_name: claude.log_name,
    };
    assert!(
        common::wait_until(BUDGET, || fresh.listing()["armed"].as_u64().unwrap_or(0)
            > 0),
        "the refreshed waiter arms; {}",
        fresh.evidence()
    );
    let name = "refreshed-hooks-41c9e7.txt";
    let id = fresh.post(&fresh.directive(name));
    fresh.done(&id, name, true);
}
