//! PRD #1666 part 2 (H2): the receipt protocol's side for a one-shot receiver
//! — Claude Code's `asyncRewake` waiter, or a hook or `inbox fetch` process —
//! which takes one batch on one connection, hands it to its agent by writing
//! it to stdout or stderr, reports each attempt, and exits.
//!
//! The sequence a one-shot runs (part 2 §1.1, §1.2): ask `may` for every
//! attempt of its batch, recording when it asked first ([`OneShotBatch::ask`]);
//! then, with nothing awaited after the grant, run the [`Fence`] immediately
//! before each block's first byte and write the blocks through a counting
//! writer ([`OneShotBatch::hand_over`]); publish each attempt's receipt
//! ([`OneShotBatch::publish`]); and only then report on the connection
//! ([`OneShotBatch::controls`]).
//!
//! A block is positively not handed only when none of its bytes was
//! accepted, or the fence refused it before its first byte. Anything else
//! that is not a whole block followed by a successful flush is possibly
//! handed, which a stage report can never turn into "not handed".
//!
//! The attempts' caps stay inside [`OneShotBatch`]: they are written to the
//! spool and nowhere else, never to a block, a control line, an error or the
//! batch's `Debug`.
//!
//! Inert: nothing calls it yet. H3 wires Claude's waiter and prompt hook to
//! it, H4 the other hooks and `inbox fetch`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::event::{
    InboxAttemptCaps, InboxAttemptRef, InboxDelivery, InboxGrant, InboxMayHandOver,
    InboxStreamControl, InboxWaitReply,
};
use crate::inbox_receipt::{GenerationSpool, RevokeState};

type Key = (String, u32, u32);

fn key(at: &InboxAttemptRef) -> Key {
    (at.id.clone(), at.a, at.e)
}

/// Which kind of one-shot hands the batch over, which decides what a block
/// written in full means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    /// Claude's `asyncRewake` waiter: a block it wrote is handed, and its
    /// commit comes later, from the prompt that carries the block.
    Waiter,
    /// A hook or `inbox fetch`: a block written in full and flushed is its
    /// commit, the strongest witness it has.
    Hook,
}

/// What became of one block on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// No byte of it was accepted, or the fence refused it before its first
    /// byte: it never reached the agent.
    NotHanded,
    /// Some of it was accepted, or all of it with no successful flush after.
    Possible,
    /// All of it was accepted and flushed.
    Written,
}

/// One batch a one-shot was handed, each block with its attempt. The caps
/// are private: see the module docs.
pub struct OneShotBatch {
    blocks: Vec<Block>,
}

struct Block {
    at: InboxAttemptRef,
    text: String,
    caps: Option<InboxAttemptCaps>,
}

impl std::fmt::Debug for OneShotBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneShotBatch")
            .field("attempts", &self.attempts())
            .finish()
    }
}

/// One piece of what a one-shot writes: framing of its own, or block `i`'s
/// bytes as the output carries them (a hook's JSON escapes them, say).
#[derive(Debug, Clone, Copy)]
pub enum Piece<'a> {
    Frame(&'a [u8]),
    Block(usize, &'a [u8]),
}

/// What [`OneShotBatch::hand_over`] did: each block's [`Output`], in batch
/// order, and the write or flush error that stopped it, if one did.
#[derive(Debug)]
pub struct HandOver {
    pub outputs: Vec<Output>,
    pub error: Option<io::Error>,
}

impl OneShotBatch {
    /// The batch of a reply's hand-outs. A hand-out without an attempt is
    /// not the receipt protocol's, and is left out.
    pub fn new(messages: Vec<InboxDelivery>) -> OneShotBatch {
        let blocks = messages
            .into_iter()
            .filter_map(|message| {
                let attempt = message.attempt?;
                Some(Block {
                    at: InboxAttemptRef {
                        id: message.id,
                        a: attempt.a,
                        e: attempt.e,
                    },
                    text: message.text,
                    caps: attempt.caps,
                })
            })
            .collect();
        OneShotBatch { blocks }
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn attempts(&self) -> Vec<InboxAttemptRef> {
        self.blocks.iter().map(|block| block.at.clone()).collect()
    }

    /// Block `i`'s text, as the daemon rendered it.
    pub fn text(&self, i: usize) -> Option<&str> {
        self.blocks.get(i).map(|block| block.text.as_str())
    }

    /// The pieces of a plain-text hand-over: `head`, then each block with
    /// `separator` between two blocks.
    pub fn plain_pieces<'a>(&'a self, head: &'a str, separator: &'a str) -> Vec<Piece<'a>> {
        let mut pieces = Vec::new();
        if !head.is_empty() {
            pieces.push(Piece::Frame(head.as_bytes()));
        }
        for (i, block) in self.blocks.iter().enumerate() {
            if i > 0 {
                pieces.push(Piece::Frame(separator.as_bytes()));
            }
            pieces.push(Piece::Block(i, block.text.as_bytes()));
        }
        pieces
    }

    /// The `may` questions for every attempt of the batch, numbered from
    /// `first_q`, one control line each. `sent_at` must be read before the
    /// first line is written, so the approval can never outlive the daemon's
    /// own grant: it lasts `grant_ttl` from then.
    pub fn ask(
        &self,
        first_q: u64,
        sent_at: Instant,
        grant_ttl: Duration,
    ) -> (Approval, Vec<String>) {
        let mut asked = BTreeMap::new();
        let mut lines = Vec::new();
        for (q, block) in (first_q..).zip(&self.blocks) {
            asked.insert(q, key(&block.at));
            lines.push(control_line(&InboxStreamControl {
                may: Some(InboxMayHandOver {
                    q,
                    id: block.at.id.clone(),
                    a: block.at.a,
                    e: block.at.e,
                }),
                ..InboxStreamControl::default()
            }));
        }
        let approval = Approval {
            valid_until: sent_at + grant_ttl,
            asked,
            granted: BTreeSet::new(),
        };
        (approval, lines)
    }

    /// Write `pieces` to `out`, running `fence` immediately before each
    /// block's first byte and dropping a block it refuses. `out` should be the
    /// raw descriptor, with no buffer between the count and the pipe. The
    /// first write error stops the hand-over; every block written in full
    /// before it, or before a failed flush, is only [`Output::Possible`].
    pub fn hand_over<W: Write>(
        &self,
        out: &mut W,
        pieces: &[Piece<'_>],
        mut fence: impl FnMut(&InboxAttemptRef) -> Result<(), FenceRefusal>,
    ) -> HandOver {
        let mut outputs = vec![Output::NotHanded; self.blocks.len()];
        let mut error = None;
        for piece in pieces {
            let (block, bytes) = match *piece {
                Piece::Frame(bytes) => (None, bytes),
                Piece::Block(i, bytes) => {
                    let Some(at) = self.blocks.get(i).map(|block| &block.at) else {
                        continue;
                    };
                    if fence(at).is_err() {
                        continue;
                    }
                    (Some(i), bytes)
                }
            };
            let (accepted, failed) = write_counted(out, bytes);
            if let Some(i) = block {
                outputs[i] = if accepted == bytes.len() {
                    Output::Written
                } else if accepted > 0 {
                    Output::Possible
                } else {
                    Output::NotHanded
                };
            }
            if failed.is_some() {
                error = failed;
                break;
            }
        }
        if error.is_none() {
            error = out.flush().err();
        }
        if error.is_some() {
            for output in &mut outputs {
                if *output == Output::Written {
                    *output = Output::Possible;
                }
            }
        }
        HandOver { outputs, error }
    }

    /// Publish each handed attempt's receipt into `spool`, before any control
    /// reports it: `handed` for every block a waiter possibly handed and for
    /// a hook's possibly handed ones, `committed` for a hook's written ones.
    /// A cap that is missing or not a cap is refused before any file is
    /// written. Answers the first failure, worded for the generation's
    /// degraded notice; the others are still tried.
    pub fn publish(
        &self,
        spool: Option<&GenerationSpool>,
        outputs: &[Output],
        receiver: Receiver,
    ) -> Option<String> {
        use crate::inbox_receipt::{Cap, RecordKey, RecordStage};
        let mut first = None;
        for (block, output) in self.blocks.iter().zip(outputs) {
            let Some(stage) = stage_of(receiver, *output) else {
                continue;
            };
            let cap = block.caps.as_ref().map(|caps| match stage {
                RecordStage::Handed => &caps.handed,
                RecordStage::Committed => &caps.commit,
            });
            let published = match (spool, cap.map(|cap| Cap::from_hex(cap))) {
                (_, None) => Err("a hand-out came without its caps".to_string()),
                (_, Some(None)) => Err("a hand-out came with a malformed cap".to_string()),
                (None, _) => Err("the receipt spool is not open".to_string()),
                (Some(spool), Some(Some(cap))) => spool
                    .publish(
                        &RecordKey {
                            msg: block.at.id.clone(),
                            a: block.at.a,
                            e: block.at.e,
                            stage,
                        },
                        &cap,
                    )
                    .map_err(|error| format!("a receipt could not be published: {error}")),
            };
            if let Err(error) = published {
                first.get_or_insert(error);
            }
        }
        first
    }

    /// The control line reporting `outputs`: each attempt as `handed`,
    /// `committed` or `not_handed`, as [`Self::publish`] recorded it, with
    /// `degraded` when a publish failed. It names attempts and nothing else.
    pub fn controls(
        &self,
        outputs: &[Output],
        receiver: Receiver,
        degraded: Option<String>,
    ) -> String {
        use crate::inbox_receipt::RecordStage;
        let mut control = InboxStreamControl {
            degraded,
            ..InboxStreamControl::default()
        };
        for (block, output) in self.blocks.iter().zip(outputs) {
            let at = block.at.clone();
            match stage_of(receiver, *output) {
                None => control.not_handed.push(at),
                Some(RecordStage::Handed) => control.handed.push(at),
                Some(RecordStage::Committed) => control.committed.push(at),
            }
        }
        control_line(&control)
    }
}

/// One control line, as the connection carries it (without its newline).
fn control_line(control: &InboxStreamControl) -> String {
    serde_json::to_string(control).unwrap_or_else(|_| "{}".to_string())
}

/// Write `bytes` to `out`, counting only what each write accepted; a write
/// interrupted before it took anything is tried again. Answers the bytes
/// accepted and the error that stopped it, if one did.
fn write_counted<W: Write>(out: &mut W, bytes: &[u8]) -> (usize, Option<io::Error>) {
    let mut accepted = 0;
    while accepted < bytes.len() {
        match out.write(&bytes[accepted..]) {
            Ok(0) => return (accepted, Some(io::ErrorKind::WriteZero.into())),
            Ok(n) => accepted += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return (accepted, Some(error)),
        }
    }
    (accepted, None)
}

/// The record stage a block's output publishes, if any.
fn stage_of(receiver: Receiver, output: Output) -> Option<crate::inbox_receipt::RecordStage> {
    use crate::inbox_receipt::RecordStage;
    match (receiver, output) {
        (_, Output::NotHanded) => None,
        (Receiver::Hook, Output::Written) => Some(RecordStage::Committed),
        (_, Output::Possible) | (Receiver::Waiter, Output::Written) => Some(RecordStage::Handed),
    }
}

/// The `may` answers a one-shot holds, and until when they hold.
#[derive(Debug)]
pub struct Approval {
    valid_until: Instant,
    asked: BTreeMap<u64, Key>,
    granted: BTreeSet<Key>,
}

impl Approval {
    /// Apply the daemon's answer to one question, read at `now`. A grant
    /// read once the approval has run out grants nothing.
    pub fn answer(&mut self, grant: &InboxGrant, now: Instant) {
        if grant.ok
            && now < self.valid_until
            && let Some(asked) = self.asked.get(&grant.q)
        {
            self.granted.insert(asked.clone());
        }
    }

    pub fn granted(&self, at: &InboxAttemptRef) -> bool {
        self.granted.contains(&key(at))
    }

    /// When the approval runs out: `grant_ttl` after the questions were
    /// sent, whenever the answers came.
    pub fn valid_until(&self) -> Instant {
        self.valid_until
    }
}

/// Whatever the daemon has sent on the one-shot's connection that can be
/// read without waiting.
pub trait Incoming {
    fn arrived(&mut self) -> Arrived;
}

/// See [`Incoming`].
#[derive(Debug, Default)]
pub struct Arrived {
    pub lines: Vec<InboxWaitReply>,
    /// The daemon closed the connection.
    pub eof: bool,
}

/// Why the fence dropped a block before its first byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FenceRefusal {
    NotGranted,
    Expired,
    Closed,
    Revoked,
    RevokeMarker,
    MarkerUnreadable(String),
}

impl std::fmt::Display for FenceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FenceRefusal::NotGranted => f.write_str("the deck did not approve this hand-over"),
            FenceRefusal::Expired => f.write_str("the deck's approval ran out"),
            FenceRefusal::Closed => f.write_str("the deck closed the connection"),
            FenceRefusal::Revoked => f.write_str("the deck revoked the hand-over"),
            FenceRefusal::RevokeMarker => f.write_str("the hand-over is marked revoked"),
            FenceRefusal::MarkerUnreadable(error) => {
                write!(
                    f,
                    "whether the hand-over was revoked cannot be read: {error}"
                )
            }
        }
    }
}

/// The check a one-shot runs, synchronously and after its last await,
/// immediately before each block's first byte (part 2 §2.5). It refuses
/// when the approval does not cover the attempt or has run out, when the
/// connection has closed or brought a revoke naming it, and when the
/// attempt's revoke marker exists or cannot be read.
pub struct Fence<'a, I> {
    approval: &'a Approval,
    spool: &'a GenerationSpool,
    incoming: I,
    closed: bool,
    revoked: BTreeSet<Key>,
}

impl<'a, I: Incoming> Fence<'a, I> {
    pub fn new(approval: &'a Approval, spool: &'a GenerationSpool, incoming: I) -> Self {
        Fence {
            approval,
            spool,
            incoming,
            closed: false,
            revoked: BTreeSet::new(),
        }
    }

    pub fn check(&mut self, at: &InboxAttemptRef, now: Instant) -> Result<(), FenceRefusal> {
        if !self.approval.granted(at) {
            return Err(FenceRefusal::NotGranted);
        }
        if now >= self.approval.valid_until() {
            return Err(FenceRefusal::Expired);
        }
        let arrived = self.incoming.arrived();
        self.closed |= arrived.eof;
        self.revoked.extend(
            arrived
                .lines
                .iter()
                .flat_map(|line| line.revoke.iter().map(key)),
        );
        if self.closed {
            return Err(FenceRefusal::Closed);
        }
        if self.revoked.contains(&key(at)) {
            return Err(FenceRefusal::Revoked);
        }
        match self.spool.revoke_state(&at.id, at.a, at.e) {
            RevokeState::NotRevoked => Ok(()),
            RevokeState::Revoked => Err(FenceRefusal::RevokeMarker),
            RevokeState::Unreadable(error) => {
                Err(FenceRefusal::MarkerUnreadable(error.to_string()))
            }
        }
    }
}

/// What the daemon's write of a once batch's line (`frame`, ending in its
/// newline) did, for every attempt in it: a waiter acts only on a whole line,
/// so a line whose newline was never accepted was never parsed. `accepted` is
/// `None` when a timeout left the progress unknown.
pub fn classify_frame(frame: &[u8], accepted: Option<usize>, flushed: bool) -> Output {
    match accepted {
        Some(n) if n < frame.len() && frame.last() == Some(&b'\n') => Output::NotHanded,
        Some(n) if n >= frame.len() && flushed => Output::Written,
        _ => Output::Possible,
    }
}

/// Open the generation spool the deck pinned for this process
/// ([`crate::inbox_receipt::ENV_INBOX_SPOOL`]), for the agent `agent_id` in
/// `pane_id`. The error is worded for the generation's degraded notice.
pub fn open_pinned(
    pinned: &Path,
    pane_id: &str,
    agent_id: &str,
) -> Result<GenerationSpool, String> {
    GenerationSpool::open(
        pinned,
        &crate::inbox::encode_token(pane_id),
        &crate::inbox::encode_token(agent_id),
    )
    .map_err(|error| format!("the receipt spool could not be opened: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use spec::spec;

    use super::*;
    use crate::event::InboxDeliveryAttempt;
    use crate::inbox::encode_token;
    use crate::inbox_receipt::{AttemptCaps, EvidenceStore, RecordKey, RecordStage, Spool};

    const PANE: &str = "2";
    const AGENT: &str = "agent-a";
    const ID1: &str = "m-k3xzq2ab-0f3c9a71d2e4b850";
    const ID2: &str = "m-k3xzq2ab-1111111111111111";
    const ID3: &str = "m-k3xzq2ab-2222222222222222";
    const TTL: Duration = Duration::from_secs(60);
    const SEP: &str = "\n\n";

    /// The daemon's side and a one-shot's side of one generation's spool.
    struct Deck {
        _state: tempfile::TempDir,
        spool: Spool,
        daemon: GenerationSpool,
        component: GenerationSpool,
        store: EvidenceStore,
    }

    fn deck() -> Deck {
        let state = crate::test_temp::tempdir().unwrap();
        let spool = Spool::start(state.path()).unwrap();
        let daemon = spool
            .generation(&encode_token(PANE), &encode_token(AGENT))
            .unwrap();
        let component = open_pinned(&spool.instance_path(), PANE, AGENT).unwrap();
        Deck {
            _state: state,
            spool,
            daemon,
            component,
            store: EvidenceStore::default(),
        }
    }

    impl Deck {
        fn generation_dir(&self, sub: &str) -> std::path::PathBuf {
            self.spool
                .instance_path()
                .join(encode_token(PANE))
                .join(encode_token(AGENT))
                .join(sub)
        }

        /// Scan until a cycle ends: what it admitted, and how many records
        /// it refused.
        fn scan(&mut self) -> (Vec<RecordKey>, usize) {
            let mut scanner = self.daemon.scanner().unwrap();
            let (mut admitted, mut refused) = (Vec::new(), 0);
            for _ in 0..8 {
                let pass = scanner.pass(&mut self.store);
                admitted.extend(pass.admitted);
                refused += pass.refused;
                if pass.ended {
                    break;
                }
            }
            admitted.sort();
            (admitted, refused)
        }
    }

    fn wire_caps(caps: &AttemptCaps) -> InboxAttemptCaps {
        InboxAttemptCaps {
            handed: caps.handed.hex(),
            commit: caps.commit.hex(),
        }
    }

    fn handout(id: &str, a: u32, e: u32, caps: Option<InboxAttemptCaps>) -> InboxDelivery {
        InboxDelivery {
            id: id.to_string(),
            subject: "subject".to_string(),
            from: "the orchestrator".to_string(),
            text: format!(
                "[dot-agent-deck message {id} delivery {a}.{e} from the orchestrator]\n\
                 body of {id}\n[end of dot-agent-deck message {id}]"
            ),
            attempt: Some(InboxDeliveryAttempt {
                a,
                e,
                d: u64::from(e),
                repeat: false,
                caps,
            }),
        }
    }

    fn at(id: &str, a: u32, e: u32) -> InboxAttemptRef {
        InboxAttemptRef {
            id: id.to_string(),
            a,
            e,
        }
    }

    fn record(id: &str, a: u32, e: u32, stage: RecordStage) -> RecordKey {
        RecordKey {
            msg: id.to_string(),
            a,
            e,
            stage,
        }
    }

    /// Ask for every attempt of `batch` at `sent_at` and answer each
    /// question `ok` at `answered`.
    fn approve(batch: &OneShotBatch, sent_at: Instant, answered: Instant) -> Approval {
        let (mut approval, lines) = batch.ask(10, sent_at, TTL);
        assert_eq!(lines.len(), batch.len(), "one question per attempt");
        for (i, line) in lines.iter().enumerate() {
            let control: InboxStreamControl = serde_json::from_str(line).unwrap();
            let may = control.may.expect("a may question");
            let asked = &batch.attempts()[i];
            assert_eq!(
                (may.q, may.id.as_str(), may.a, may.e),
                (10 + i as u64, asked.id.as_str(), asked.a, asked.e)
            );
            approval.answer(
                &InboxGrant {
                    q: may.q,
                    ok: true,
                    refusal: None,
                },
                answered,
            );
        }
        approval
    }

    /// What the deck has sent, released one read at a time.
    #[derive(Default)]
    struct Script(VecDeque<Arrived>);

    impl Incoming for Script {
        fn arrived(&mut self) -> Arrived {
            self.0.pop_front().unwrap_or_default()
        }
    }

    fn revoke(at: InboxAttemptRef) -> Arrived {
        Arrived {
            lines: vec![InboxWaitReply {
                revoke: vec![at],
                ..InboxWaitReply::default()
            }],
            eof: false,
        }
    }

    fn eof() -> Arrived {
        Arrived {
            lines: Vec::new(),
            eof: true,
        }
    }

    enum Step {
        Take(usize),
        Fail(io::ErrorKind),
    }

    /// A pipe that accepts what its script says, then up to `chunk` bytes a
    /// write.
    struct Pipe {
        steps: VecDeque<Step>,
        chunk: usize,
        written: Vec<u8>,
        flush_fails: bool,
    }

    fn pipe(steps: Vec<Step>) -> Pipe {
        Pipe {
            steps: steps.into(),
            chunk: usize::MAX,
            written: Vec::new(),
            flush_fails: false,
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = match self.steps.pop_front() {
                Some(Step::Take(n)) => n,
                Some(Step::Fail(kind)) => return Err(kind.into()),
                None => self.chunk,
            }
            .min(buf.len());
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fails {
                Err(io::ErrorKind::Other.into())
            } else {
                Ok(())
            }
        }
    }

    fn two_blocks() -> OneShotBatch {
        OneShotBatch::new(vec![handout(ID1, 1, 1, None), handout(ID2, 1, 1, None)])
    }

    fn hand(batch: &OneShotBatch, out: &mut Pipe) -> HandOver {
        batch.hand_over(out, &batch.plain_pieces("", SEP), |_| Ok(()))
    }

    /// Scenario: A one-shot opens the spool the deck pinned for its pane and
    /// agent, and publishes the receipts of a batch it handed over: a hook's
    /// written block, a hook's block whose commit cap is really its hand-out
    /// cap, a waiter's written block, and blocks whose cap is not a cap or
    /// missing. The daemon's scan admits the genuine commit and hand-out,
    /// refuses the commit carried on the hand-out cap, and finds no file for
    /// a block whose cap was refused; a relative or non-instance pinned path
    /// and a missing spool are each reported as a degraded failure.
    #[spec("inbox/receipt/020")]
    #[test]
    fn receipt_020_a_one_shot_publishes_on_the_attempts_own_cap() {
        let mut deck = deck();
        let relative = open_pinned(
            Path::new("inbox-spool/0123456789abcdef0123456789abcdef"),
            PANE,
            AGENT,
        )
        .unwrap_err();
        assert!(relative.contains("absolute"), "{relative}");
        let not_instance = open_pinned(
            &deck.spool.instance_path().with_file_name("not-an-instance"),
            PANE,
            AGENT,
        )
        .unwrap_err();
        assert!(not_instance.contains("instance"), "{not_instance}");

        let genuine = deck.store.issue(ID1, 1, 1).unwrap();
        let mut forged = wire_caps(&deck.store.issue(ID2, 1, 1).unwrap());
        forged.commit = forged.handed.clone();
        let hook = OneShotBatch::new(vec![
            handout(ID1, 1, 1, Some(wire_caps(&genuine))),
            handout(ID2, 1, 1, Some(forged)),
        ]);
        let written = [Output::Written, Output::Written];
        assert_eq!(
            hook.publish(Some(&deck.component), &written, Receiver::Hook),
            None
        );
        let waited = deck.store.issue(ID3, 1, 1).unwrap();
        let waiter = OneShotBatch::new(vec![handout(ID3, 1, 1, Some(wire_caps(&waited)))]);
        assert_eq!(
            waiter.publish(Some(&deck.component), &[Output::Written], Receiver::Waiter),
            None
        );
        let (admitted, refused) = deck.scan();
        assert_eq!(
            admitted,
            vec![
                record(ID1, 1, 1, RecordStage::Committed),
                record(ID3, 1, 1, RecordStage::Handed),
            ]
        );
        assert_eq!(refused, 1, "a commit on the hand-out cap is refused");

        let rec = deck.generation_dir("rec");
        let files = || std::fs::read_dir(&rec).unwrap().count();
        let before = files();
        let not_a_cap = InboxAttemptCaps {
            handed: "zz".repeat(16),
            commit: "zz".repeat(16),
        };
        for (what, batch) in [
            (
                "not a cap",
                OneShotBatch::new(vec![handout(ID1, 1, 2, Some(not_a_cap))]),
            ),
            ("no cap", OneShotBatch::new(vec![handout(ID1, 1, 3, None)])),
        ] {
            for receiver in [Receiver::Hook, Receiver::Waiter] {
                let degraded = batch.publish(Some(&deck.component), &[Output::Written], receiver);
                assert!(degraded.is_some(), "{what}: {receiver:?}");
            }
        }
        assert_eq!(files(), before, "a refused cap writes no file");
        assert!(
            hook.publish(None, &written, Receiver::Hook).is_some(),
            "no spool is degraded, not silent"
        );
        assert_eq!(
            hook.publish(
                Some(&deck.component),
                &[Output::NotHanded, Output::NotHanded],
                Receiver::Hook
            ),
            None,
            "a block not handed publishes nothing"
        );
        assert_eq!(files(), before);
    }

    /// Scenario: Approve a two-block batch, then check its fence against an
    /// attempt it never asked for, at and past the approval's end (also when
    /// the approval came back late), after the deck closed the connection,
    /// after a revoke line, after a revoke marker was written and when the
    /// marker cannot be read; then hand the batch over while the deck's
    /// revoke for block two arrives between the two blocks. Every one of
    /// those refuses the block before its first byte, and block one's fence
    /// does not cover block two: block one is written, block two is not.
    #[spec("inbox/receipt/021")]
    #[test]
    fn receipt_021_the_fence_refuses_before_the_first_byte_and_covers_one_block() {
        let deck = deck();
        let batch = two_blocks();
        let [one, two] = <[InboxAttemptRef; 2]>::try_from(batch.attempts()).unwrap();
        let t = Instant::now();
        let approval = approve(&batch, t, t);
        assert_eq!(approval.valid_until(), t + TTL);
        let mut fence = Fence::new(&approval, &deck.component, Script::default());
        assert_eq!(fence.check(&one, t), Ok(()));
        assert_eq!(
            fence.check(&at(ID1, 1, 2), t),
            Err(FenceRefusal::NotGranted)
        );
        assert_eq!(
            fence.check(&one, t + TTL - Duration::from_millis(1)),
            Ok(())
        );
        assert_eq!(fence.check(&one, t + TTL), Err(FenceRefusal::Expired));

        let late = approve(&batch, t, t + TTL - Duration::from_millis(1));
        assert_eq!(late.valid_until(), t + TTL, "counted from the question");
        let mut fence = Fence::new(&late, &deck.component, Script::default());
        assert_eq!(fence.check(&one, t + TTL), Err(FenceRefusal::Expired));
        let too_late = approve(&batch, t, t + TTL);
        assert!(!too_late.granted(&one), "an answer read after the end");
        let mut refused = OneShotBatch::new(vec![]).ask(1, t, TTL).0;
        refused.answer(
            &InboxGrant {
                q: 1,
                ok: true,
                refusal: None,
            },
            t,
        );
        assert!(
            !refused.granted(&one),
            "an answer to no question grants nothing"
        );

        let mut fence = Fence::new(&approval, &deck.component, Script(vec![eof()].into()));
        assert_eq!(fence.check(&one, t), Err(FenceRefusal::Closed));
        assert_eq!(
            fence.check(&two, t),
            Err(FenceRefusal::Closed),
            "it stays closed"
        );
        let mut fence = Fence::new(
            &approval,
            &deck.component,
            Script(vec![revoke(one.clone())].into()),
        );
        assert_eq!(fence.check(&one, t), Err(FenceRefusal::Revoked));
        assert_eq!(fence.check(&two, t), Ok(()));
        assert_eq!(
            fence.check(&one, t),
            Err(FenceRefusal::Revoked),
            "it stays revoked"
        );

        deck.daemon.write_revoke(ID1, 1, 1).unwrap();
        let mut fence = Fence::new(&approval, &deck.component, Script::default());
        assert_eq!(fence.check(&one, t), Err(FenceRefusal::RevokeMarker));
        assert_eq!(fence.check(&two, t), Ok(()));
        std::fs::create_dir(deck.generation_dir("revoked").join(format!("{ID2}.1.1"))).unwrap();
        assert!(matches!(
            fence.check(&two, t),
            Err(FenceRefusal::MarkerUnreadable(_))
        ));

        let deck = deck_with_no_markers();
        let mut out = pipe(vec![]);
        let mut fence = Fence::new(
            &approval,
            &deck.component,
            Script(vec![Arrived::default(), revoke(two.clone())].into()),
        );
        let handed = batch.hand_over(&mut out, &batch.plain_pieces("", SEP), |at| {
            fence.check(at, t)
        });
        assert_eq!(handed.outputs, vec![Output::Written, Output::NotHanded]);
        assert_eq!(
            String::from_utf8(out.written).unwrap(),
            format!("{}{SEP}", batch.text(0).unwrap()),
            "no byte of the refused block"
        );
        deck.daemon.write_revoke(ID1, 1, 1).unwrap();
        let mut out = pipe(vec![]);
        let mut fence = Fence::new(&approval, &deck.component, Script::default());
        let handed = batch.hand_over(&mut out, &batch.plain_pieces("", SEP), |at| {
            fence.check(at, t)
        });
        assert_eq!(handed.outputs, vec![Output::NotHanded, Output::Written]);
        assert!(
            !String::from_utf8(out.written).unwrap().contains(ID1),
            "the marked block never reached the pipe"
        );
    }

    fn deck_with_no_markers() -> Deck {
        deck()
    }

    /// Scenario: Hand a two-block batch to pipes that accept everything a
    /// few bytes at a time, nothing, a prefix and then an error, all of it
    /// and then fail the flush, block one and then an error, a prefix and
    /// then a timeout, an interrupted write, and a write of zero bytes; and
    /// classify the daemon's write of a once line that lost its newline,
    /// took all of it but failed the flush, took all of it, or timed out
    /// with its progress unknown. Only a block accepted whole and flushed is
    /// written, only one of which no byte was accepted is not handed, and
    /// everything else is possibly handed.
    #[spec("inbox/receipt/022")]
    #[test]
    fn receipt_022_only_a_zero_byte_block_is_not_handed() {
        use Output::{NotHanded, Possible, Written};
        let batch = two_blocks();
        let len = batch.text(0).unwrap().len();
        let full = format!("{}{SEP}{}", batch.text(0).unwrap(), batch.text(1).unwrap());

        let mut out = pipe(vec![]);
        out.chunk = 4;
        let handed = hand(&batch, &mut out);
        assert_eq!(handed.outputs, vec![Written, Written]);
        assert!(handed.error.is_none());
        assert_eq!(
            String::from_utf8(out.written).unwrap(),
            full,
            "each accepted byte counted, none assumed"
        );

        let cases: Vec<(&str, Vec<Step>, bool, Vec<Output>)> = vec![
            (
                "nothing accepted",
                vec![Step::Fail(io::ErrorKind::BrokenPipe)],
                false,
                vec![NotHanded, NotHanded],
            ),
            (
                "a prefix, then an error",
                vec![Step::Take(3), Step::Fail(io::ErrorKind::Other)],
                false,
                vec![Possible, NotHanded],
            ),
            (
                "everything, then a failed flush",
                vec![],
                true,
                vec![Possible, Possible],
            ),
            (
                "block one, then an error on the separator",
                vec![Step::Take(len), Step::Fail(io::ErrorKind::BrokenPipe)],
                false,
                vec![Possible, NotHanded],
            ),
            (
                "block one, then an error on block two's first byte",
                vec![
                    Step::Take(len),
                    Step::Take(SEP.len()),
                    Step::Fail(io::ErrorKind::BrokenPipe),
                ],
                false,
                vec![Possible, NotHanded],
            ),
            (
                "a prefix, then a timeout",
                vec![Step::Take(2), Step::Fail(io::ErrorKind::TimedOut)],
                false,
                vec![Possible, NotHanded],
            ),
            (
                "an interrupted write is tried again",
                vec![Step::Fail(io::ErrorKind::Interrupted)],
                false,
                vec![Written, Written],
            ),
            (
                "a write of zero bytes",
                vec![Step::Take(0)],
                false,
                vec![NotHanded, NotHanded],
            ),
        ];
        for (what, steps, flush_fails, expected) in cases {
            let mut out = pipe(steps);
            out.flush_fails = flush_fails;
            let handed = hand(&batch, &mut out);
            assert_eq!(handed.outputs, expected, "{what}");
            assert_eq!(
                handed.error.is_some(),
                expected != vec![Written, Written],
                "{what}"
            );
        }

        let frame = b"{\"messages\":[]}\n";
        let n = frame.len();
        assert_eq!(classify_frame(frame, Some(0), false), NotHanded);
        assert_eq!(
            classify_frame(frame, Some(n - 1), true),
            NotHanded,
            "a line whose newline never went was never parsed"
        );
        assert_eq!(classify_frame(frame, Some(n), false), Possible);
        assert_eq!(classify_frame(frame, Some(n), true), Written);
        assert_eq!(classify_frame(frame, None, false), Possible);
        assert_eq!(
            classify_frame(b"no newline", Some(3), true),
            Possible,
            "without its newline nothing says it was not parsed"
        );
    }

    /// Scenario: Build a batch from hand-outs whose caps are known, and
    /// render every public thing the library produces for it: the block
    /// texts, the `may` lines, the control line for each output as a hook
    /// and as a waiter with and without a degraded publish, every fence
    /// refusal, a hand-over's error, the pinned-spool errors, and the
    /// batch's and the approval's `Debug`. No cap's hex appears in any of
    /// them.
    #[spec("inbox/receipt/025")]
    #[test]
    fn receipt_025_no_cap_leaves_the_library() {
        let mut deck = deck();
        let one = deck.store.issue(ID1, 1, 1).unwrap();
        let two = deck.store.issue(ID2, 1, 1).unwrap();
        let caps: Vec<String> = [&one, &two]
            .iter()
            .flat_map(|caps| [caps.handed.hex(), caps.commit.hex()])
            .collect();
        let batch = OneShotBatch::new(vec![
            handout(ID1, 1, 1, Some(wire_caps(&one))),
            handout(ID2, 1, 1, Some(wire_caps(&two))),
        ]);
        let t = Instant::now();
        let mut public: Vec<String> = Vec::new();
        public.extend((0..batch.len()).map(|i| batch.text(i).unwrap().to_string()));
        let (approval, lines) = batch.ask(1, t, TTL);
        public.extend(lines);
        public.push(format!("{batch:?}"));
        public.push(format!("{approval:?}"));
        let degraded = batch.publish(None, &[Output::Written, Output::Written], Receiver::Hook);
        assert!(degraded.is_some());
        public.extend(degraded.clone());
        for receiver in [Receiver::Hook, Receiver::Waiter] {
            for outputs in [
                [Output::Written, Output::Possible],
                [Output::NotHanded, Output::Written],
            ] {
                for degraded in [None, degraded.clone()] {
                    let line = batch.controls(&outputs, receiver, degraded);
                    let control: InboxStreamControl = serde_json::from_str(&line).unwrap();
                    assert_eq!(
                        control.handed.len() + control.committed.len() + control.not_handed.len(),
                        2,
                        "{line}"
                    );
                    public.push(line);
                }
            }
        }
        for refusal in [
            FenceRefusal::NotGranted,
            FenceRefusal::Expired,
            FenceRefusal::Closed,
            FenceRefusal::Revoked,
            FenceRefusal::RevokeMarker,
            FenceRefusal::MarkerUnreadable("permission denied".to_string()),
        ] {
            public.push(refusal.to_string());
            public.push(format!("{refusal:?}"));
        }
        let mut out = pipe(vec![Step::Take(3), Step::Fail(io::ErrorKind::BrokenPipe)]);
        let handed = hand(&batch, &mut out);
        public.push(format!("{handed:?}"));
        public.extend(handed.error.map(|error| error.to_string()));
        public.extend(open_pinned(Path::new("relative"), PANE, AGENT).err());
        public.extend(open_pinned(&deck.spool.instance_path().join("nope"), PANE, AGENT).err());
        assert!(public.len() > 20);
        for text in &public {
            for cap in &caps {
                assert!(
                    !text.to_lowercase().contains(cap),
                    "a cap leaked into {text:?}"
                );
            }
        }
    }
}
