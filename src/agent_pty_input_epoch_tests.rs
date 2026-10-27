//! PRD #1666 part 3 (Q35-06): the input epoch on the real PTY writer, the
//! real output hook and the registry's retirement paths.

use super::*;
use crate::composer_probe::epoch::{Enqueued, GenerationInput, Writer};
use crate::composer_probe::query_ledger::QueryKind;
use spec::spec;

/// One `write` call's bytes and the epoch the writer found.
type Seen = (Vec<u8>, u64);

/// A PTY writer that records, for each `write` call, its bytes and the epoch
/// it found; it takes nothing while its gate is shut, after saying it got a
/// write.
struct EpochWitness {
    input: Arc<GenerationInput>,
    seen: Arc<Mutex<Vec<Seen>>>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: std::sync::mpsc::Sender<()>,
}

impl std::io::Write for EpochWitness {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.entered.send(());
        let (open, wake) = &*self.gate;
        let mut open = open.lock().unwrap();
        while !*open {
            open = wake.wait(open).unwrap();
        }
        drop(open);
        let epoch = self.input.epoch().epoch();
        self.seen.lock().unwrap().push((buf.to_vec(), epoch));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Witnessed {
    input: Arc<GenerationInput>,
    seen: Arc<Mutex<Vec<Seen>>>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    entered: std::sync::mpsc::Receiver<()>,
}

impl Witnessed {
    fn new(open: bool) -> (Self, EpochWitness) {
        let input = Arc::new(GenerationInput::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(open), std::sync::Condvar::new()));
        let (tx, entered) = std::sync::mpsc::channel();
        let witness = EpochWitness {
            input: Arc::clone(&input),
            seen: Arc::clone(&seen),
            gate: Arc::clone(&gate),
            entered: tx,
        };
        (
            Self {
                input,
                seen,
                gate,
                entered,
            },
            witness,
        )
    }

    fn open(&self) {
        let (open, wake) = &*self.gate;
        *open.lock().unwrap() = true;
        wake.notify_all();
    }

    /// The epoch the witness saw at the write of `bytes`.
    fn seen_at(&self, bytes: &[u8]) -> Option<u64> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(b, _)| b == bytes)
            .map(|&(_, e)| e)
    }
}

fn recorder() -> InputRecorder {
    InputRecorder {
        pane_id_env: None,
        state: Arc::new(Mutex::new(PaneInputState::default())),
        retired: Arc::new(AtomicBool::new(false)),
        doorbell: None,
    }
}

/// Which constructor `op` was built with. Exhaustive with no `_` arm: a new
/// op has to be given a number here, and then listed by [`every_op`].
fn constructor(op: &PtyOp) -> usize {
    match op {
        PtyOp::Write(_, ByteSource::Deck | ByteSource::User | ByteSource::Answer) => 0,
        PtyOp::WriteAll(_, ByteSource::Deck | ByteSource::User | ByteSource::Answer, _) => 1,
        PtyOp::Answer(..) => 2,
        PtyOp::Flush => 3,
        PtyOp::Surrender(_) => 4,
    }
}

/// Every production op constructor with every byte source, labelled, and
/// whether it carries bytes.
fn every_op(answers: &QuestionAnswers) -> Vec<(String, PtyOp)> {
    let mut ops = Vec::new();
    for source in [ByteSource::Deck, ByteSource::User, ByteSource::Answer] {
        ops.push((
            format!("Write {source:?}"),
            PtyOp::Write(format!("write-{source:?}").into_bytes(), source),
        ));
        for flush in [false, true] {
            ops.push((
                format!("WriteAll {source:?} {flush}"),
                PtyOp::WriteAll(
                    format!("all-{source:?}-{flush}").into_bytes(),
                    source,
                    flush,
                ),
            ));
        }
    }
    ops.push((
        "Answer".to_string(),
        PtyOp::Answer(b"answer".to_vec(), answers.answer_gate("p", "q", None)),
    ));
    ops.push(("Flush".to_string(), PtyOp::Flush));
    ops
}

/// The epoch writer the op should move the epoch as.
fn expected_writer(op: &PtyOp) -> Option<Writer> {
    match op {
        PtyOp::Write(_, s) | PtyOp::WriteAll(_, s, _) => Some(match s {
            ByteSource::Deck => Writer::UnnamedDeck,
            ByteSource::User => Writer::Human,
            ByteSource::Answer => Writer::Answer,
        }),
        PtyOp::Answer(..) => Some(Writer::Answer),
        PtyOp::Flush | PtyOp::Surrender(_) => None,
    }
}

/// Scenario: Queue every PTY op the daemon builds, with every byte source, on a writer thread still busy with an earlier write, then let it run. Each op that carries bytes has moved the epoch, as foreign input, by the time its submit returns, while it is still queued; a flush moves nothing; and the writer finds the moved epoch when it writes the op's bytes.
#[spec("inbox/probe/001")]
#[test]
fn probe_001_every_op_moves_the_epoch_before_the_writer_sees_it() {
    let answers = QuestionAnswers::default();
    let ops = every_op(&answers);
    let mut listed: Vec<usize> = ops.iter().map(|(_, op)| constructor(op)).collect();
    listed.sort_unstable();
    listed.dedup();
    assert_eq!(
        listed,
        [0, 1, 2, 3],
        "every production constructor is listed"
    );
    for (label, op) in ops {
        let (w, witness) = Witnessed::new(false);
        let pty = PtyWriterThread::spawn(Box::new(witness), recorder(), Arc::clone(&w.input));
        let (tx, _blocker_done) = std::sync::mpsc::sync_channel(1);
        pty.submit(
            PtyOp::Write(b"blocker".to_vec(), ByteSource::User),
            PtyReply::Blocking(tx),
        )
        .expect("the writer thread runs");
        w.entered.recv().unwrap();
        let before = w.input.epoch();
        assert_eq!(before.epoch(), 1, "{label}: the blocker moved it");
        let writer = expected_writer(&op);
        let bytes = match &op {
            PtyOp::Write(b, _) | PtyOp::WriteAll(b, _, _) | PtyOp::Answer(b, _) => b.clone(),
            _ => Vec::new(),
        };
        let (tx, done) = std::sync::mpsc::sync_channel(1);
        pty.submit(op, PtyReply::Blocking(tx))
            .expect("the writer thread runs");
        let queued = w.input.epoch();
        match writer {
            Some(_) => {
                assert_eq!(queued.epoch(), 2, "{label}: moved while still queued");
                assert_eq!(queued.owner(), None, "{label}: no deck job named");
                assert_eq!(queued.last_foreign(), 2, "{label}: foreign");
                assert!(queued.touched(), "{label}");
            }
            None => assert_eq!(queued, before, "{label}: carries no bytes"),
        }
        w.open();
        done.recv().unwrap();
        if writer.is_some() && label != "Answer" {
            assert_eq!(
                w.seen_at(&bytes),
                Some(2),
                "{label}: the writer saw the move"
            );
        }
        assert_eq!(w.input.epoch().epoch(), queued.epoch(), "{label}: once");
    }
}

/// Scenario: Write through each of the pane writer's paths: a person's frame with and without an ingress ticket, the `std::io::Write` impl, a voice answer and a doorbell transaction's deck job. Each moves the epoch once, as foreign input, before the writer takes its bytes.
#[spec("inbox/probe/001")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_001_every_pane_writer_path_moves_the_epoch() {
    use std::io::Write as _;
    let answers = QuestionAnswers::default();
    let (w, witness) = Witnessed::new(true);
    let mut writer = PaneWriter::new(
        Box::new(witness),
        None,
        Arc::new(Mutex::new(PaneInputState::default())),
        Arc::new(AtomicBool::new(false)),
        None,
        Arc::clone(&w.input),
    );
    writer.write_user(b"unticketed").await.unwrap();
    assert_eq!(w.seen_at(b"unticketed"), Some(1));
    let ledger = crate::ingress_ledger::IngressLedger::new();
    writer
        .write_user_ticketed(b"ticketed", Some(ledger.admit()))
        .await
        .unwrap();
    assert_eq!(w.seen_at(b"ticketed"), Some(2));
    tokio::task::block_in_place(|| writer.write_all(b"io-write")).unwrap();
    assert_eq!(w.seen_at(b"io-write"), Some(3));
    let answered = writer
        .write_answer(b"answer", answers.answer_gate("p", "q", None), || {}, None)
        .await
        .unwrap();
    assert!(
        matches!(answered, AnswerWrite::Superseded),
        "no question was raised"
    );
    assert_eq!(
        w.input.epoch().epoch(),
        4,
        "a superseded answer writes nothing and still moved it"
    );
    let mut job = writer
        .queue_deck_job(b"deck".to_vec(), Arc::default())
        .expect("the writer thread runs");
    job.done().await.expect("the deck job ran");
    assert_eq!(w.seen_at(b"deck"), Some(5));
    let epoch = w.input.epoch();
    assert_eq!(
        (epoch.epoch(), epoch.last_foreign(), epoch.owner()),
        (5, 5, None)
    );
}

/// Scenario: Queue a person's write behind one the PTY isn't taking, with a bound that runs out, so the job is taken back before it starts. The epoch moved when it was queued and stays moved, though its bytes are never written.
#[spec("inbox/probe/001")]
#[tokio::test]
async fn probe_001_a_withdrawn_job_still_moved_the_epoch() {
    let (w, witness) = Witnessed::new(false);
    let pty = PtyWriterThread::spawn(Box::new(witness), recorder(), Arc::clone(&w.input));
    let (tx, _done) = std::sync::mpsc::sync_channel(1);
    pty.submit(
        PtyOp::Write(b"blocker".to_vec(), ByteSource::User),
        PtyReply::Blocking(tx),
    )
    .unwrap();
    w.entered.recv().unwrap();
    let outcome = pty
        .run(
            PtyOp::WriteAll(b"withdrawn".to_vec(), ByteSource::User, true),
            Some(Duration::from_millis(20)),
        )
        .await;
    assert!(matches!(outcome, PtyJobOutcome::Withdrawn));
    assert_eq!(w.input.epoch().epoch(), 2);
    w.open();
    pty.until_idle().await;
    assert_eq!(w.seen_at(b"withdrawn"), None, "never written");
    assert!(w.input.touched());
}

/// Scenario: Through the registry, type into a real `cat` pane's agent, then deliver that agent's prompt report and turn edges; on a second pane deliver the hooks first and type after. The box is touched after the typing in both orders, and no hook changes the epoch.
#[cfg(unix)]
#[spec("inbox/probe/002")]
#[tokio::test]
async fn probe_002_a_prompt_hook_never_empties_a_touched_box() {
    use crate::composer_model::PromptReport;
    let registry = Arc::new(AgentPtyRegistry::new());
    for (pane, hooks_first) in [
        ("probe-002-touch-first", false),
        ("probe-002-hooks-first", true),
    ] {
        let id = spawn_on(&registry, pane, "/bin/cat");
        let input = Arc::clone(registry.inner.lock().agents[&id].bus.input());
        let hooks = || {
            registry.note_prompt_report(pane, &id, PromptReport::User("typed"));
            registry.note_turn(pane, &id, TurnEdge::Started);
            registry.note_prompt_report(pane, &id, PromptReport::Deck);
            registry.note_turn(pane, &id, TurnEdge::Ended);
        };
        if hooks_first {
            hooks();
            assert_eq!(
                input.epoch(),
                Default::default(),
                "{pane}: hooks move nothing"
            );
        }
        let writer = registry.agent_writer(&id).unwrap();
        writer.lock().await.write_user(b"typed").await.unwrap();
        let typed = input.epoch();
        assert!(typed.touched(), "{pane}");
        if !hooks_first {
            hooks();
        }
        assert_eq!(input.epoch(), typed, "{pane}: still touched, unchanged");
    }
}

/// Scenario: Push a CPR query, then a `?2026h` frame, through the buses of a Codex-profiled, a Claude-profiled and an unprofiled generation. `B` holds after the query on all three; the frame ends it on Codex only, whose profile declares that its CPR waits end at its next frame.
#[spec("inbox/probe/060")]
#[test]
fn probe_060_each_profile_declares_how_b_ends() {
    use crate::doorbell_screen::{CLAUDE_PROFILE, CODEX_PROFILE};
    for (profile, label, ends) in [
        (Some(&CODEX_PROFILE), "codex", true),
        (Some(&CLAUDE_PROFILE), "claude", false),
        (None, "none", false),
    ] {
        let bus = AgentBus::profiled(profile, 24, 80);
        assert!(!bus.input().backlog(), "{label}");
        bus.push(b"\x1b[6n".to_vec());
        assert!(bus.input().backlog(), "{label}: the query");
        bus.push(b"\x1b[?2026h\x1b[Hframe".to_vec());
        assert_eq!(!bus.input().backlog(), ends, "{label}: the frame");
    }
}

/// Scenario: Close a generation through the registry after its output asked for a cursor position and it accepted a native Ctrl+G. Its ledger, `B` and hazard FIFO are dropped, it notes and numbers nothing afterwards, its FIFO never reads clear, and its epoch still counts what is written.
#[spec("inbox/probe/052")]
#[test]
fn probe_052_retirement_drops_the_generation_structures() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let now = Instant::now();
    let id = registry.insert_test_agent_for_pane(Box::new(IdleTestChild), Some("probe-052"));
    let bus = registry.inner.lock().agents[&id].bus.clone();
    bus.push(b"\x1b[6n".to_vec());
    assert_eq!(bus.input().accept_native(now), Some(1));
    assert!(!bus.input().hazards_clear(now));
    registry.close_agent(&id).unwrap();
    let input = bus.input();
    assert_eq!(input.hazards(now), (Vec::new(), 0));
    assert!(!input.backlog());
    assert_eq!(input.reply_class(QueryKind::Cpr), (false, false));
    bus.push(b"\x1b[6n".to_vec());
    assert!(!input.backlog(), "a retired generation notes nothing");
    assert_eq!(input.accept_native(now), None);
    assert!(!input.hazards_clear(now), "never clear once retired");
    assert!(matches!(
        input.enqueue(Writer::Human, b"x"),
        Enqueued::Moved(1)
    ));
}

/// Scenario: With the agent's focus reporting on, queue a whole focus report as each byte source's `WriteAll` and as a voice answer. Only a person's report is exempt and leaves the epoch where it was; the deck's and a voice answer's move it.
#[spec("inbox/probe/001")]
#[test]
fn probe_001_each_byte_source_is_its_epoch_writer() {
    use crate::composer_probe::epoch::OutputNote;
    let answers = QuestionAnswers::default();
    for (label, op, exempt) in [
        (
            "deck",
            PtyOp::WriteAll(b"\x1b[I".to_vec(), ByteSource::Deck, true),
            false,
        ),
        (
            "user",
            PtyOp::WriteAll(b"\x1b[I".to_vec(), ByteSource::User, true),
            true,
        ),
        (
            "answer source",
            PtyOp::Write(b"\x1b[I".to_vec(), ByteSource::Answer),
            false,
        ),
        (
            "answer",
            PtyOp::Answer(b"\x1b[I".to_vec(), answers.answer_gate("p", "q", None)),
            false,
        ),
    ] {
        let (w, witness) = Witnessed::new(true);
        w.input.on_output(
            &[OutputNote::Mode {
                mode: 1004,
                on: true,
            }],
            Instant::now(),
        );
        let pty = PtyWriterThread::spawn(Box::new(witness), recorder(), Arc::clone(&w.input));
        let (tx, done) = std::sync::mpsc::sync_channel(1);
        pty.submit(op, PtyReply::Blocking(tx)).unwrap();
        done.recv().unwrap();
        assert_eq!(w.input.epoch().epoch(), u64::from(!exempt), "{label}");
    }
}

/// Spawn `command` on `pane` with the pane id in its environment.
#[cfg(unix)]
fn spawn_on(registry: &Arc<AgentPtyRegistry>, pane: &str, command: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some(command),
            env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                pane.to_string(),
            )]),
            ..SpawnOptions::default()
        })
        .expect("spawn on the pane")
}

/// Scenario: Restart a `cat` pane through the registry, and on another pane spawn a new agent over one whose process (`sh -c 'exit 0'`) already exited, after each predecessor accepted a native Ctrl+G. Both predecessors' hazard FIFOs are dropped and they number nothing afterwards, while each successor starts with an empty FIFO of its own.
#[cfg(unix)]
#[spec("inbox/probe/052")]
#[tokio::test]
async fn probe_052_a_successor_retires_its_predecessor_input() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let now = Instant::now();
    let input_of = |id: &str| Arc::clone(registry.inner.lock().agents[id].bus.input());

    let old = input_of(&spawn_on(&registry, "probe-052-respawn", "/bin/cat"));
    assert_eq!(old.accept_native(now), Some(1));
    let successor = registry
        .respawn_agent_for_pane("probe-052-respawn", "/bin/cat")
        .await
        .expect("respawn");
    assert_eq!(old.hazards(now), (Vec::new(), 0), "respawn");
    assert_eq!(old.accept_native(now), None, "respawn");
    let new = input_of(&successor);
    assert!(!Arc::ptr_eq(&old, &new));
    assert!(new.hazards_clear(now), "a fresh FIFO");

    let pane = "probe-052-exited";
    let exited = spawn_on(&registry, pane, "sh -c 'exit 0'");
    let old = input_of(&exited);
    assert_eq!(old.accept_native(now), Some(1));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !registry.pane_occupant_has_exited(pane) {
        assert!(Instant::now() < deadline, "the predecessor exits");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        registry.inner.lock().agents.contains_key(&exited),
        "the exited record is still on the pane"
    );
    assert!(!old.hazards_clear(now), "exiting alone retires nothing");
    let successor = spawn_on(&registry, pane, "/bin/cat");
    assert_eq!(
        old.hazards(now),
        (Vec::new(), 0),
        "spawn over an exited agent"
    );
    assert_eq!(old.accept_native(now), None);
    assert!(input_of(&successor).hazards_clear(now));
}

/// Scenario: Spawn a real `cat` pane through the registry, push a DA1 query through its bus, and write a whole DA1 reply through its pane writer, then a second. The spawned generation's bus and writer share one input: the first reply is exempt and the second touches.
#[cfg(unix)]
#[spec("inbox/probe/045")]
#[tokio::test]
async fn probe_045_a_spawned_generation_shares_one_input() {
    let registry = Arc::new(AgentPtyRegistry::new());
    let id = spawn_on(&registry, "probe-045-spawned", "/bin/cat");
    let bus = registry.inner.lock().agents[&id].bus.clone();
    bus.push(b"\x1b[c".to_vec());
    assert_eq!(bus.input().reply_class(QueryKind::Da1), (true, true));
    let writer = registry.agent_writer(&id).unwrap();
    writer
        .lock()
        .await
        .write_user(b"\x1b[?62;22c")
        .await
        .unwrap();
    assert!(!bus.input().touched(), "exempt");
    writer
        .lock()
        .await
        .write_user(b"\x1b[?62;22c")
        .await
        .unwrap();
    assert!(bus.input().touched(), "a duplicate touches");
}
