#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! Lane-2 proof that the per-agent prompt keys the deck serves (PRD #1541)
//! work against a REAL interactive agent: interrupt its turn, clear its
//! prompt, delete characters from it.
//!
//! The keys come from the daemon's own `ListAgents` answer
//! (`AgentRecord::prompt_keys`, stamped from `agent_registry` by
//! `attach_prompt_keys`), never from the registry directly, so a test that
//! passes here proves what a client is actually handed. They are written the
//! way the desktop writes them: raw `KIND_STREAM_IN` frames on an attach
//! stream to the agent (`desktop/src-tauri/src/terminal.rs` `write`), not
//! keystrokes through the TUI, which has key handling of its own.
//!
//! Claude Code on Haiku has a test per key, asserted against its framed input
//! row. Codex and OpenCode (each on its cheap test model) have one test each
//! that runs all three keys in one session, asserted on the whole pane, since
//! their input boxes are drawn differently. Pi is not covered — see the
//! catalog's `prompt/voice-keys` section for why.
//!
//! `voice/reading-reply/002` reuses the interactive Claude harness to prove
//! that a genuine Stop hook delivers the final reply to a subscribed client.

mod common;

use std::time::{Duration, Instant};

use common::TuiDeck;
use dot_agent_deck::agent_pty::AgentRecord;
use dot_agent_deck::agent_registry::{ClearPresses, PromptKeys};
use spec::spec;

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// What the desktop presses for a `per_wrapped_row` clear (PRD #1541): more
/// than any voice-typed prompt wraps to, and harmless on an empty prompt.
const PER_WRAPPED_ROW_PRESSES: usize = 64;
/// What the desktop presses for a `per_line` clear.
const PER_LINE_PRESSES: usize = 16;
/// How long typed text must sit before Enter: an Enter that arrives in the
/// same burst as the text is taken as part of a paste and inserts a newline.
const BEFORE_SUBMIT: Duration = Duration::from_millis(750);

/// Claude's live editor is a single row between two horizontal rules. Scope
/// assertions to that row so text in the transcript cannot satisfy them.
fn claude_input_row(grid: &str) -> Option<String> {
    let rows: Vec<&str> = grid.lines().collect();
    let rule = |row: &str| row.contains("────────────────────");
    rows.iter().enumerate().find_map(|(index, row)| {
        let framed = index
            .checked_sub(1)
            .and_then(|above| rows.get(above))
            .is_some_and(|r| rule(r))
            && rows.get(index + 1).is_some_and(|r| rule(r))
            && !rule(row);
        framed.then(|| row.to_string())
    })
}

/// The text typed in Claude's input row: what follows its `❯` prompt marker,
/// less the padding and any frame the deck draws around the pane.
fn claude_input_text(grid: &str) -> Option<String> {
    let row = claude_input_row(grid)?;
    let (_, typed) = row.split_once('❯')?;
    let padding = |c: char| c.is_whitespace() || c == '\u{a0}' || c == '│';
    Some(typed.trim_matches(padding).to_string())
}

/// The served keys, written on one attach stream to the agent the way the
/// desktop's terminal bridge writes them ([`common::AttachInput`]).
struct PaneWriter {
    input: common::AttachInput,
}

impl PaneWriter {
    fn attach(socket: &std::path::Path, agent_id: &str) -> Self {
        Self {
            input: common::AttachInput::attach(socket, agent_id),
        }
    }

    fn write(&mut self, bytes: &str) {
        self.input.write(bytes);
    }

    /// The interrupt steps, in order, each followed by its pause.
    fn interrupt(&mut self, keys: &PromptKeys) {
        self.input.write_steps(keys.interrupt.iter().map(|step| {
            (
                step.bytes.as_ref(),
                Duration::from_millis(u64::from(step.pause_after_ms)),
            )
        }));
    }

    /// The clear key, pressed as many times as the desktop presses it for the
    /// key's rule, in writes no larger than the key allows and the served
    /// pause apart. Separate writes are not separate reads: measured against
    /// Claude Code 2.1.289 through this path, two writes of 32 Ctrl+U sent back
    /// to back left the prompt untouched exactly as one write of 64 does, and
    /// so did two 250 ms apart on a box at load 174; two 1 s apart cleared it.
    /// That is why the deck serves the pause rather than each sender guessing.
    fn clear(&mut self, keys: &PromptKeys) -> usize {
        let presses = match keys.clear.presses {
            ClearPresses::PerWrappedRow => PER_WRAPPED_ROW_PRESSES,
            ClearPresses::PerLine => PER_LINE_PRESSES,
            ClearPresses::Unknown => panic!("the deck served a clear rule this build cannot count"),
        };
        let per_write = keys
            .clear
            .max_presses_per_write
            .map_or(presses, |cap| cap as usize);
        let mut chunks = Vec::new();
        let mut left = presses;
        while left > 0 {
            let now = left.min(per_write);
            chunks.push(keys.clear.bytes.repeat(now));
            left -= now;
        }
        let writes = chunks.len();
        let between =
            Duration::from_millis(u64::from(keys.clear.pause_between_writes_ms.unwrap_or(0)));
        self.input
            .write_steps(chunks.iter().map(|chunk| (chunk.as_str(), between)));
        writes
    }
}

/// A launched interactive Claude Haiku pane, its daemon record and the keys
/// the daemon served for it.
struct LiveClaude {
    deck: TuiDeck,
    agent_id: String,
    keys: PromptKeys,
}

impl LiveClaude {
    fn launch(name: &str, sentinel: Option<&str>) -> Self {
        let deck = TuiDeck::builder()
            .with_pty_size(160, 45)
            .with_imported_claude_credentials()
            .launch_with_fixture("minimal");
        deck.wait_for_string("No active agents");
        if let Some(sentinel) = sentinel {
            std::fs::write(deck.workdir().join(sentinel), "prompt keys sentinel\n")
                .expect("write the sentinel file");
        }
        let cwd = deck.workdir().to_path_buf();
        let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
        if let Ok(canonical) = cwd.canonicalize() {
            let canonical = canonical.to_string_lossy().into_owned();
            if !trust_paths.contains(&canonical) {
                trust_paths.push(canonical);
            }
        }
        common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
            .expect("seed Claude onboarding and per-folder trust");

        deck.send_keys(b"\x0e");
        deck.wait_for_string("Select Directory");
        deck.send_keys(b" ");
        deck.wait_for_string("┌ New Agent");
        deck.send_keys(b"\t");
        deck.send_keys(name.as_bytes());
        deck.send_keys(b"\t");
        deck.send_keys(format!("claude --model {HAIKU_MODEL} --allowedTools Bash Read").as_bytes());
        let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
        deck.click(submit_col, submit_row);
        assert!(
            deck.wait_for_grid_string_within("Claude Code v", Duration::from_secs(45)),
            "the genuine interactive Claude UI must render; grid:\n{}",
            deck.snapshot_grid()
        );
        deck.wait_for_string("[Command Mode Ctrl+D]");
        deck.wait_until_grid("Claude's empty input row", |grid| {
            claude_input_row(grid).is_some()
        });

        let record = record_named(&deck, name);
        let keys = record.prompt_keys.clone().unwrap_or_else(|| {
            panic!("the daemon served no prompt_keys for a Claude Code agent; record={record:?}")
        });
        for step in keys.interrupt.iter() {
            assert!(
                !step.bytes.contains('\u{3}'),
                "an interrupt step must never be Ctrl+C: {keys:?}"
            );
        }
        Self {
            agent_id: record.id,
            deck,
            keys,
        }
    }

    fn writer(&self) -> PaneWriter {
        PaneWriter::attach(self.deck.attach_socket_path(), &self.agent_id)
    }

    /// The agent is still registered with the daemon and has not crashed.
    fn assert_still_running(&self, after: &str) {
        let records = common::agent_records_on(self.deck.attach_socket_path());
        let record = records
            .iter()
            .find(|record| record.id == self.agent_id)
            .unwrap_or_else(|| panic!("the Claude agent is gone after {after}: {records:?}"));
        assert_ne!(
            record.crashed,
            Some(true),
            "the Claude agent crashed after {after}: {record:?}"
        );
        assert!(
            !self.deck.snapshot_grid().contains("No active agents"),
            "the Claude pane was torn down after {after}:\n{}",
            self.deck.snapshot_grid()
        );
    }

    /// Type `prompt`, wait for it to land in the input row, then press Enter
    /// as a write of its own.
    fn submit(&self, writer: &mut PaneWriter, prompt: &str, marker: &str) {
        writer.write(prompt);
        // The whole grid rather than the input row: a long prompt wraps onto
        // more than one row, which the one-row matcher does not frame.
        self.deck.wait_until_grid_then_hold(
            "the prompt settled in Claude's input box",
            BEFORE_SUBMIT,
            |grid| grid.contains(marker),
        );
        writer.write("\r");
    }

    fn wait_for_input_text(&self, what: &str, text: &str) {
        self.deck.wait_until_grid(what, |grid| {
            claude_input_text(grid).as_deref() == Some(text)
        });
    }
}

/// Scenario: Open a real interactive Claude Haiku pane in a fixture containing a uniquely named sentinel file and subscribe to that agent's future turn replies. Ask Claude to list the files and report the sentinel filename; its genuine Stop hook must deliver a successful final reply containing the filename to the client, and the reply must be visible in the attached pane.
#[spec("voice/reading-reply/002")]
#[test]
fn reading_reply_002_real_claude_stop_delivers_sentinel_to_subscribed_client() {
    use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};

    skip_unless!(common::check_claude_available());

    const SENTINEL: &str = "reading_reply_sentinel_7c4e.txt";
    let claude = LiveClaude::launch("reading-reply", Some(SENTINEL));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("turn reply client runtime");
    let client = DaemonClient::new(claude.deck.attach_socket_path().to_path_buf());
    let GatedQuery::Answered(mut replies) = runtime
        .block_on(client.subscribe_turn_replies(&claude.agent_id))
        .expect("subscribe to real Claude turn replies before submitting the prompt")
    else {
        panic!("the current daemon must advertise turn-replies");
    };

    // The full filename is absent from the prompt: Claude must discover it
    // from the fixture, rather than echo a filename provided by the test.
    let mut writer = claude.writer();
    claude.submit(
        &mut writer,
        "Use the Bash tool to run ls -1 in the current directory. Then reply with the full \
         filename that starts with reading_reply_sentinel, verbatim. That is the whole task; \
         do not change any files.",
        "Use the Bash tool",
    );
    let reply = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(180), replies.next_reply())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the genuine Claude Stop hook delivered no turn reply within 180s; grid:\n{}",
                    claude.deck.snapshot_grid()
                )
            })
            .expect("decode the real Claude final reply")
            .expect("turn reply subscription must stay open until the turn ends")
    });
    assert_eq!(reply.agent_id, claude.agent_id);
    assert!(
        !reply.reply.failed,
        "the real Claude turn failed: {reply:?}"
    );
    assert!(
        reply.reply.text.contains(SENTINEL),
        "the hook's final reply must contain the discovered sentinel {SENTINEL:?}: {reply:?}"
    );
    assert!(
        claude
            .deck
            .wait_for_grid_string_within(SENTINEL, Duration::from_secs(30)),
        "the real agent's sentinel reply must also be visible in its attached pane:\n{}",
        claude.deck.snapshot_grid()
    );
    claude.assert_still_running("the completed reading turn");
    claude.deck.wait_until_grid_then_hold(
        "the real Claude reply remains visible for the recording",
        Duration::from_secs(2),
        |grid| grid.contains(SENTINEL),
    );
}

/// The highest number that opens a line of the pane — how far a numbered list
/// the agent is writing has got. `0` when none is on screen.
fn highest_listed_number(grid: &str) -> u32 {
    grid.lines()
        .flat_map(|line| line.split(['│', '┃']))
        .filter_map(|cell| {
            let cell = cell.trim_start_matches(|c: char| c.is_whitespace() || c == '⏺');
            let digits: String = cell.chars().take_while(char::is_ascii_digit).collect();
            let rest = &cell[digits.len()..];
            (!digits.is_empty() && rest.starts_with(['.', ')', ':', ' ']))
                .then(|| digits.parse().ok())
                .flatten()
        })
        .max()
        .unwrap_or(0)
}

fn record_named(deck: &TuiDeck, suffix: &str) -> AgentRecord {
    let records = common::agent_records_on(deck.attach_socket_path());
    records
        .iter()
        .find(|record| {
            record
                .display_name
                .as_deref()
                .is_some_and(|name| name.ends_with(suffix))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no agent whose name ends with {suffix:?}: {records:?}"))
}

/// Scenario: Start a genuine interactive Claude Haiku pane, read the
/// interrupt key the daemon serves for it, and give it a long numbered-list
/// task. Once it is visibly working, write the interrupt steps on an attach
/// stream as the desktop does; the turn must stop with the agent still running,
/// and a follow-up prompt asking for a uniquely named fixture file is answered.
#[spec("prompt/voice-keys/001")]
#[test]
fn prompt_voice_keys_001_interrupt_stops_a_live_claude_turn_and_keeps_the_agent() {
    skip_unless!(common::check_claude_available());

    const SENTINEL: &str = "voicekeys_interrupt_sentinel_4e7b.txt";
    let claude = LiveClaude::launch("voicekeys-interrupt", Some(SENTINEL));
    let mut writer = claude.writer();

    claude.submit(
        &mut writer,
        "Write the numbers from 1 to 600, each on its own line followed by one full sentence \
         about that number. Do not use any tools and do not stop early.",
        "Write the numbers",
    );
    // Visibly working: the numbered list is streaming into the pane. (An
    // interrupt that lands before any output makes Claude put the unsent
    // prompt back in its input box instead of printing "Interrupted", which is
    // a different and less telling picture.)
    assert!(
        claude
            .deck
            .wait_for_grid_predicate_within(Duration::from_secs(90), |grid| {
                grid.contains("esc to interrupt") && highest_listed_number(grid) >= 5
            }),
        "Claude never showed it was working on the long task:\n{}",
        claude.deck.snapshot_grid()
    );

    writer.interrupt(&claude.keys);
    assert!(
        claude
            .deck
            .wait_for_grid_predicate_within(Duration::from_secs(20), |grid| {
                grid.contains("Interrupted") && !grid.contains("esc to interrupt")
            }),
        "the served interrupt key did not end Claude's turn:\n{}",
        claude.deck.snapshot_grid()
    );
    // Stopped, not paused: the working indicator stays away and the list
    // stops growing.
    let reached = highest_listed_number(&claude.deck.snapshot_grid());
    claude.deck.wait_until_grid_then_hold(
        "Claude stays stopped after the interrupt",
        Duration::from_secs(3),
        |grid| !grid.contains("esc to interrupt") && highest_listed_number(grid) <= reached,
    );
    assert!(
        reached < 600,
        "the turn finished rather than being interrupted:\n{}",
        claude.deck.snapshot_grid()
    );
    claude.assert_still_running("the interrupt");

    claude.submit(
        &mut writer,
        "Use the Bash tool to run `ls` in the current directory, then reply with the full name \
         of the file whose name starts with voicekeys_interrupt_sentinel, verbatim. That is the \
         whole task.",
        "Use the Bash tool",
    );
    assert!(
        claude
            .deck
            .wait_for_grid_string_within(SENTINEL, Duration::from_secs(120)),
        "Claude did not answer the follow-up prompt after the interrupt:\n{}",
        claude.deck.snapshot_grid()
    );
    claude.assert_still_running("the follow-up prompt");
}

/// Scenario: Start a genuine interactive Claude Haiku pane, type a draft into
/// its prompt through an attach stream, then write the clear key the daemon
/// serves as many times as the desktop presses it, in writes no larger than the
/// served cap. The input row must be empty, a word typed after it must stand
/// alone, and the agent must still be running.
#[spec("prompt/voice-keys/002")]
#[test]
fn prompt_voice_keys_002_clear_empties_a_live_claude_prompt() {
    skip_unless!(common::check_claude_available());

    const DRAFT: &str = "voicekeys_clear_draft_9c2d with a few more words to clear";
    const AFTER: &str = "voicekeys_after_clear_1a5f";
    let claude = LiveClaude::launch("voicekeys-clear", None);
    let mut writer = claude.writer();

    writer.write(DRAFT);
    claude.wait_for_input_text("the draft in Claude's input row", DRAFT);

    let writes = writer.clear(&claude.keys);
    assert_eq!(
        claude.keys.clear.presses,
        ClearPresses::PerWrappedRow,
        "Claude Code's clear is one press per wrapped row"
    );
    assert_eq!(
        writes, 2,
        "64 presses in writes of at most {:?}",
        claude.keys.clear.max_presses_per_write
    );
    assert_eq!(
        claude.keys.clear.pause_between_writes_ms,
        Some(1000),
        "the deck serves the gap Claude needs between two clear writes"
    );
    claude
        .deck
        .wait_until_grid("Claude's input row empty after the clear", |grid| {
            claude_input_text(grid).is_some_and(|text| !text.contains("voicekeys_clear_draft"))
                && !grid.contains("voicekeys_clear_draft")
        });

    writer.write(AFTER);
    claude.wait_for_input_text("only the word typed after the clear", AFTER);
    claude.assert_still_running("the clear");
}

/// Scenario: Start a genuine interactive Claude Haiku pane, type a kept word
/// and a dictated tail into its prompt through an attach stream, then write the
/// delete key the daemon serves once per tail character in one write. Exactly
/// the tail must go: a word typed straight after reads contiguously after the
/// kept word and its space. Then a 500-character dictation is removed the same
/// way, in one write of 500 deletes, and the agent is still running.
#[spec("prompt/voice-keys/003")]
#[test]
fn prompt_voice_keys_003_delete_char_removes_exactly_the_last_characters() {
    skip_unless!(common::check_claude_available());

    const KEPT: &str = "voicekeys_keep_6d0e ";
    const TAIL: &str = "voicekeys dictated tail 8b31 ";
    const NEXT: &str = "voicekeys_next_2f94";
    let claude = LiveClaude::launch("voicekeys-delete", None);
    let mut writer = claude.writer();

    writer.write(KEPT);
    writer.write(TAIL);
    claude.wait_for_input_text(
        "the kept word and the tail in Claude's input row",
        format!("{KEPT}{TAIL}").trim_end(),
    );

    writer.write(&claude.keys.delete_char.bytes.repeat(TAIL.chars().count()));
    writer.write(NEXT);
    claude.wait_for_input_text(
        "exactly the tail removed — no more and no less",
        &format!("{KEPT}{NEXT}"),
    );

    // A long dictation, as long as voice ever scratches without nearing the
    // 800-character paste collapse, removed by one write of deletes. Written
    // only once the prompt before it is on screen: two writes read together
    // are one paste to Claude, and 808 characters would collapse.
    let long = long_dictation(LONG_SCRATCH_CHARS);
    writer.write(&long);
    claude.deck.wait_until_grid_then_hold(
        "the long dictation settled in Claude's input box",
        BEFORE_SUBMIT,
        |grid| grid.contains(LONG_MARKER) && !grid.contains("[Pasted"),
    );
    writer.write(&claude.keys.delete_char.bytes.repeat(long.chars().count()));
    writer.write(FINAL);
    claude.wait_for_input_text(
        "exactly the long dictation removed — no more and no less",
        &format!("{KEPT}{NEXT}{FINAL}"),
    );
    claude.assert_still_running("the deletes");
}

/// How long `prompt/voice-keys/003`'s long scratch is, in characters.
const LONG_SCRATCH_CHARS: usize = 500;
/// The start of the long dictation, which shows it reached the input box.
const LONG_MARKER: &str = "voicekeys_longtail_5e2b";
/// What is written after the long dictation is deleted.
const FINAL: &str = "voicekeys_final_7c19";

/// A dictated write of exactly `chars` characters, as voice writes one: words
/// and a trailing space, opening with [`LONG_MARKER`].
fn long_dictation(chars: usize) -> String {
    let mut text = format!(" {LONG_MARKER} ");
    while text.chars().count() < chars {
        text.push_str("and some more dictated words ");
    }
    text.chars().take(chars - 1).chain([' ']).collect()
}

/// How one non-Claude agent is started in the harness, and how it is driven.
struct AgentLaunch {
    /// The suffix the pane's name ends with.
    name: &'static str,
    /// A string the agent paints once its input box is ready for typing.
    ready: &'static str,
    /// Whether Enter has to be repeated until the input box empties — Codex
    /// drops a submit that lands while it is still initialising
    /// (`codex_hooks_001` measured it).
    resubmit_until_empty: bool,
}

/// The freshly built binary's directory ahead of `PATH`, so an agent's own
/// `dot-agent-deck` calls resolve to the build under test.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let dir = std::path::Path::new(bin)
        .parent()
        .expect("test binary has a parent dir")
        .to_str()
        .expect("binary directory is UTF-8");
    format!("{dir}:{}", std::env::var("PATH").unwrap_or_default())
}

/// Start `command` through the New agent dialog of a deck built by `builder`,
/// wait for its input box, and read the keys the daemon serves for it.
fn launch_agent(
    builder: common::TuiDeckBuilder,
    fixture: &str,
    launch: &AgentLaunch,
    command: &str,
    sentinel: &str,
) -> (TuiDeck, String, PromptKeys) {
    let deck = builder.with_pty_size(180, 45).launch_with_fixture(fixture);
    deck.wait_for_string("No active agents");
    std::fs::write(deck.workdir().join(sentinel), "prompt keys sentinel\n")
        .expect("write the sentinel file");
    deck.send_keys(b"\x0e");
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" ");
    deck.wait_for_string("┌ New Agent");
    deck.send_keys(b"\t");
    deck.send_keys(launch.name.as_bytes());
    deck.send_keys(b"\t");
    deck.send_keys(command.as_bytes());
    let (submit_col, submit_row) = deck.wait_for_in_grid("[Submit]");
    deck.click(submit_col, submit_row);
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within(launch.ready, Duration::from_secs(90)),
        "{} never painted {:?}:\n{}",
        launch.name,
        launch.ready,
        deck.snapshot_grid()
    );
    let record = record_named(&deck, launch.name);
    let keys = record
        .prompt_keys
        .clone()
        .unwrap_or_else(|| panic!("the daemon served no prompt_keys: {record:?}"));
    for step in keys.interrupt.iter() {
        assert!(!step.bytes.contains('\u{3}'), "never Ctrl+C: {keys:?}");
    }
    (deck, record.id, keys)
}

fn assert_running(deck: &TuiDeck, agent_id: &str, after: &str) {
    let records = common::agent_records_on(deck.attach_socket_path());
    let record = records
        .iter()
        .find(|record| record.id == agent_id)
        .unwrap_or_else(|| panic!("the agent is gone after {after}: {records:?}"));
    assert_ne!(
        record.crashed,
        Some(true),
        "crashed after {after}: {record:?}"
    );
}

/// Delete, clear and interrupt against one live agent, asserted on the whole
/// pane, then a follow-up prompt that must name `sentinel`.
fn exercise_prompt_keys(
    launch: &AgentLaunch,
    deck: &TuiDeck,
    agent_id: &str,
    keys: &PromptKeys,
    sentinel: &str,
) {
    let mut writer = PaneWriter::attach(deck.attach_socket_path(), agent_id);
    let submit = |writer: &mut PaneWriter, prompt: &str, marker: &str| {
        writer.write(prompt);
        deck.wait_until_grid_then_hold(
            "the prompt settled in the input box",
            BEFORE_SUBMIT,
            |grid| grid.contains(marker),
        );
        writer.write("\r");
        if launch.resubmit_until_empty {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !deck.wait_for_grid_string_within(launch.ready, Duration::from_secs(2)) {
                assert!(
                    Instant::now() < deadline,
                    "the prompt never left the input box:\n{}",
                    deck.snapshot_grid()
                );
                writer.write("\r");
            }
        }
    };

    // Delete: exactly the tail goes.
    const KEPT: &str = "voicekeys_keep_6d0e ";
    const TAIL: &str = "voicekeys dictated tail 8b31 ";
    const NEXT: &str = "voicekeys_next_2f94";
    writer.write(KEPT);
    writer.write(TAIL);
    deck.wait_until_grid_then_hold("the kept word and the tail", BEFORE_SUBMIT, |grid| {
        grid.contains("tail 8b31")
    });
    writer.write(&keys.delete_char.bytes.repeat(TAIL.chars().count()));
    writer.write(NEXT);
    let joined = format!("{KEPT}{NEXT}");
    deck.wait_until_grid_then_hold("exactly the tail removed", BEFORE_SUBMIT, |grid| {
        grid.contains(&joined) && !grid.contains("tail 8b31")
    });

    // Clear: nothing of the prompt is left.
    writer.clear(keys);
    deck.wait_until_grid("the prompt cleared", |grid| {
        !grid.contains("voicekeys_keep_6d0e") && !grid.contains(NEXT)
    });
    const AFTER: &str = "voicekeys_after_clear_1a5f";
    writer.write(AFTER);
    deck.wait_until_grid_then_hold("a word typed after the clear", BEFORE_SUBMIT, |grid| {
        grid.contains(AFTER)
    });
    writer.clear(keys);
    deck.wait_until_grid("the prompt cleared again", |grid| !grid.contains(AFTER));
    assert_running(deck, agent_id, "the clear");

    // Interrupt: the list stops growing and the agent keeps running.
    submit(
        &mut writer,
        "Write the numbers from 1 to 600 as a numbered list, each followed by one full sentence \
         about that number. Do not use any tools and do not stop early.",
        "Write the numbers",
    );
    assert!(
        deck.wait_for_grid_predicate_within(
            Duration::from_secs(120),
            |grid| highest_listed_number(grid) >= 5
        ),
        "the agent never visibly worked on the long task:\n{}",
        deck.snapshot_grid()
    );
    writer.interrupt(keys);
    // Stopped: the list's highest number holds for 4 s running.
    let reached = std::cell::Cell::new(highest_listed_number(&deck.snapshot_grid()));
    let stable_since = std::cell::Cell::new(Instant::now());
    assert!(
        deck.wait_for_grid_predicate_within(Duration::from_secs(30), |grid| {
            let now = highest_listed_number(grid);
            if now > reached.get() {
                reached.set(now);
                stable_since.set(Instant::now());
            }
            stable_since.get().elapsed() >= Duration::from_secs(4)
        }),
        "the list kept growing after the interrupt:\n{}",
        deck.snapshot_grid()
    );
    let reached = reached.get();
    assert!(
        reached < 600,
        "the turn finished rather than being interrupted:\n{}",
        deck.snapshot_grid()
    );
    assert_running(deck, agent_id, "the interrupt");

    // The follow-up is answered.
    submit(
        &mut writer,
        "Run `ls` in the current directory with your shell tool, then reply with the full name of \
         the file whose name starts with voicekeys_parity_sentinel, verbatim. That is the whole task.",
        "Run `ls`",
    );
    assert!(
        deck.wait_for_grid_string_within(sentinel, Duration::from_secs(180)),
        "the agent did not answer the follow-up prompt after the interrupt:\n{}",
        deck.snapshot_grid()
    );
    assert_running(deck, agent_id, "the follow-up prompt");
}

/// Scenario: Start a genuine interactive Codex pane on the cheap test model,
/// read the keys the daemon serves for it, and write them on an attach stream
/// as the desktop does: delete removes exactly a dictated tail, clear empties
/// the prompt, interrupt stops a long numbered list mid-way, and a follow-up
/// prompt naming a fixture file is answered with the agent still running.
#[spec("prompt/voice-keys/004")]
#[test]
fn prompt_voice_keys_004_codex_honours_the_served_keys() {
    skip_unless!(common::check_codex_available());

    const SENTINEL: &str = "voicekeys_parity_sentinel_codex_3c8a.txt";
    let launch = AgentLaunch {
        name: "voicekeys-codex",
        ready: "Ask Codex to do anything",
        resubmit_until_empty: true,
    };
    let command = format!(
        "codex --model {} --sandbox workspace-write --ask-for-approval never -c 'model_reasoning_effort=\"low\"'",
        common::codex_test_model()
    );
    let builder = TuiDeck::builder()
        .with_env("PATH", path_with_binary_dir())
        .with_imported_codex_credentials();
    let (deck, agent_id, keys) = launch_agent(builder, "codex-live", &launch, &command, SENTINEL);
    exercise_prompt_keys(&launch, &deck, &agent_id, &keys, SENTINEL);
}

/// Scenario: Start a genuine interactive OpenCode pane on the cheap test model,
/// read the keys the daemon serves for it, and write them on an attach stream
/// as the desktop does — the interrupt's two Esc writes with the served pause
/// between them: delete removes exactly a dictated tail, clear empties the
/// prompt, interrupt stops a long numbered list mid-way, and a follow-up prompt
/// naming a fixture file is answered with the agent still running.
#[spec("prompt/voice-keys/005")]
#[test]
fn prompt_voice_keys_005_opencode_honours_the_served_keys() {
    skip_unless!(common::check_opencode_available());

    const SENTINEL: &str = "voicekeys_parity_sentinel_opencode_7f15.txt";
    let launch = AgentLaunch {
        name: "voicekeys-opencode",
        ready: "Ask anything",
        resubmit_until_empty: false,
    };
    let command = format!("opencode --model {}", common::opencode_test_model());
    let builder = TuiDeck::builder().with_imported_opencode_credentials();
    let (deck, agent_id, keys) = launch_agent(builder, "minimal", &launch, &command, SENTINEL);
    assert_eq!(
        keys.interrupt.len(),
        2,
        "OpenCode's interrupt is Esc, a pause, Esc: {keys:?}"
    );
    exercise_prompt_keys(&launch, &deck, &agent_id, &keys, SENTINEL);
}
