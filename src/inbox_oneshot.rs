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
//! The attempts' cap fields are private to [`OneShotBatch`]: the library
//! writes them to the spool, and puts them in none of the blocks, control
//! lines, errors of its own or the batch's `Debug`. It does not inspect text
//! it is given and passes on as it is — a message's text, a `degraded`
//! string, an underlying writer's error — so it makes no claim about what
//! that text holds.
//!
//! Claude Code's waiter and prompt hook run it since H3 ([`Connection`] is
//! their side of the socket); H4 wires the other hooks and `inbox fetch`.

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

    /// The message ids of the batch, in order.
    pub fn ids(&self) -> Vec<String> {
        self.blocks
            .iter()
            .map(|block| block.at.id.clone())
            .collect()
    }

    /// Part 2 §1.1: a once-waiter's batch carries no caps; each attempt's
    /// are read from its own caps file, by its exact `(id, A, e)`. Answers
    /// the first attempt whose caps could not be read, worded for the
    /// generation's degraded notice; that attempt publishes nothing.
    pub fn caps_from(&mut self, spool: &GenerationSpool) -> Option<String> {
        use crate::inbox_receipt::CapsLookup;
        let mut first = None;
        for block in &mut self.blocks {
            match spool.read_caps_checked(&block.at.id, block.at.a, block.at.e) {
                CapsLookup::Found(caps) => {
                    block.caps = Some(InboxAttemptCaps {
                        handed: caps.handed.hex(),
                        commit: caps.commit.hex(),
                    });
                }
                CapsLookup::Missing => {
                    first
                        .get_or_insert_with(|| "a delivery's receipt caps are missing".to_string());
                }
                CapsLookup::Malformed => {
                    first.get_or_insert_with(|| {
                        "a delivery's receipt caps are malformed".to_string()
                    });
                }
                CapsLookup::Failed(error) => {
                    first.get_or_insert_with(|| {
                        format!("a delivery's receipt caps could not be read: {error}")
                    });
                }
            }
        }
        first
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
    /// block's first byte and dropping every piece of a block it refuses.
    /// `out` should be the raw descriptor, with no buffer between the count
    /// and the pipe. A block may come in several pieces: its outcome counts
    /// the bytes accepted across all of them, so it is [`Output::NotHanded`]
    /// only when none of its bytes was accepted, and [`Output::Written`] only
    /// when every byte of every piece was. A block's later pieces are not
    /// fenced again: once its first byte was accepted it is possibly handed,
    /// and stopping it there would only cut it short. The first write error
    /// stops the hand-over; every block written in full before it, or before
    /// a failed flush, is only [`Output::Possible`].
    pub fn hand_over<W: Write>(
        &self,
        out: &mut W,
        pieces: &[Piece<'_>],
        mut fence: impl FnMut(&InboxAttemptRef) -> Result<(), FenceRefusal>,
    ) -> HandOver {
        #[derive(Clone, Copy, Default)]
        struct Progress {
            len: usize,
            accepted: usize,
            fenced: Option<bool>,
        }
        let mut progress = vec![Progress::default(); self.blocks.len()];
        for piece in pieces {
            if let Piece::Block(i, bytes) = *piece
                && let Some(block) = progress.get_mut(i)
            {
                block.len += bytes.len();
            }
        }
        let mut error = None;
        for piece in pieces {
            let (block, bytes) = match *piece {
                Piece::Frame(bytes) => (None, bytes),
                Piece::Block(i, bytes) => {
                    let Some(at) = self.blocks.get(i).map(|block| &block.at) else {
                        continue;
                    };
                    if bytes.is_empty() {
                        continue;
                    }
                    let passed = *progress[i].fenced.get_or_insert_with(|| fence(at).is_ok());
                    if !passed {
                        continue;
                    }
                    (Some(i), bytes)
                }
            };
            let (accepted, failed) = write_counted(out, bytes);
            if let Some(i) = block {
                progress[i].accepted += accepted;
            }
            if failed.is_some() {
                error = failed;
                break;
            }
        }
        if error.is_none() {
            error = out.flush().err();
        }
        let outputs = progress
            .iter()
            .map(|block| {
                if block.accepted == 0 {
                    Output::NotHanded
                } else if block.accepted == block.len && error.is_none() {
                    Output::Written
                } else {
                    Output::Possible
                }
            })
            .collect();
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
    /// A newer receiver of the same agent replaced this one.
    Superseded,
    /// The deck sent a terminal refusal on the connection: its stable code
    /// (`not_in_deck` for a caller that is not a live agent of the deck).
    Refused(String),
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
            FenceRefusal::Superseded => f.write_str("a newer receiver replaced this one"),
            FenceRefusal::Refused(code) => write!(f, "the deck refused the connection: {code}"),
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
/// connection has closed or brought a terminal line (`superseded`,
/// `not_in_deck`, a refusal) or a revoke naming it, and when the attempt's
/// revoke marker exists or cannot be read. A close or a terminal line,
/// once seen, refuses every later block too.
pub struct Fence<'a, I> {
    approval: &'a Approval,
    spool: Option<&'a GenerationSpool>,
    incoming: I,
    ended: Option<FenceRefusal>,
    revoked: BTreeSet<Key>,
}

impl<'a, I: Incoming> Fence<'a, I> {
    pub fn new(approval: &'a Approval, spool: &'a GenerationSpool, incoming: I) -> Self {
        Fence {
            approval,
            spool: Some(spool),
            incoming,
            ended: None,
            revoked: BTreeSet::new(),
        }
    }

    /// [`Self::new`] for a one-shot of a deck that pinned no spool
    /// ([`Pinned::Unpinned`]): there is no revoke marker to read, since that
    /// deck writes none, so only the approval and the connection fence it.
    pub fn unpinned(approval: &'a Approval, incoming: I) -> Self {
        Fence {
            approval,
            spool: None,
            incoming,
            ended: None,
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
        for line in &arrived.lines {
            self.revoked.extend(line.revoke.iter().map(key));
            if self.ended.is_none() {
                self.ended = terminal(line);
            }
        }
        if arrived.eof && self.ended.is_none() {
            self.ended = Some(FenceRefusal::Closed);
        }
        if let Some(ended) = &self.ended {
            return Err(ended.clone());
        }
        if self.revoked.contains(&key(at)) {
            return Err(FenceRefusal::Revoked);
        }
        let Some(spool) = self.spool else {
            return Ok(());
        };
        match spool.revoke_state(&at.id, at.a, at.e) {
            RevokeState::NotRevoked => Ok(()),
            RevokeState::Revoked => Err(FenceRefusal::RevokeMarker),
            RevokeState::Unreadable(error) => {
                Err(FenceRefusal::MarkerUnreadable(error.to_string()))
            }
        }
    }
}

/// The refusal a line that ends the connection's use stands for, if it is
/// one.
fn terminal(line: &InboxWaitReply) -> Option<FenceRefusal> {
    if line.superseded {
        Some(FenceRefusal::Superseded)
    } else if line.not_in_deck {
        Some(FenceRefusal::Refused("not_in_deck".to_string()))
    } else {
        line.refused.clone().map(FenceRefusal::Refused)
    }
}

/// One line read from the daemon by [`read_frame`].
#[derive(Debug)]
pub enum Frame {
    /// A whole line, without its newline.
    Line(Vec<u8>),
    /// The connection ended between lines.
    Eof,
    /// The connection ended inside a line. Its bytes are dropped unread.
    Unterminated,
    /// The line ran past the bound before its newline.
    TooLong,
    Failed(io::Error),
}

/// Read one line from the daemon, at most `max` bytes before its newline. A
/// line is handed back only once its newline has been read: the bytes before
/// an end of connection inside a line are dropped, never parsed, even when
/// they would parse (`BufRead::read_line`, which today's legacy waiter uses,
/// hands them back). That is what [`classify_frame`] relies on.
pub fn read_frame<R: io::BufRead>(reader: &mut R, max: usize) -> Frame {
    let mut line = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Frame::Failed(error),
        };
        if available.is_empty() {
            return if line.is_empty() {
                Frame::Eof
            } else {
                Frame::Unterminated
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let taken = newline.map_or(available.len(), |at| at + 1);
        if line.len() + taken - usize::from(newline.is_some()) > max {
            return Frame::TooLong;
        }
        line.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if newline.is_some() {
            line.pop();
            return Frame::Line(line);
        }
    }
}

/// What the daemon's write of a once batch's line (`frame`, ending in its
/// newline) did, for every attempt in it. A line whose newline was never
/// accepted is [`Output::NotHanded`]: the peer cannot have read that
/// newline, and a receiver reading through [`read_frame`] drops a line
/// without one unparsed. That holds only for such a receiver, so the daemon
/// may use this only on a connection whose receiver declared the one-shot
/// integration (part 2 §2.7, H3's waiter); today's legacy waiter reads with
/// `read_line`, which would parse such a line, and is never classified this
/// way. `accepted` is `None` when a timeout left the progress unknown.
pub fn classify_frame(frame: &[u8], accepted: Option<usize>, flushed: bool) -> Output {
    match accepted {
        Some(n) if n < frame.len() && frame.last() == Some(&b'\n') => Output::NotHanded,
        Some(n) if n >= frame.len() && flushed => Output::Written,
        _ => Output::Possible,
    }
}

/// The generation spool the deck pinned for this process, as a one-shot
/// finds it.
#[derive(Debug)]
pub enum Pinned {
    Open(GenerationSpool),
    /// The deck pinned no spool: it runs without one, and says so on the
    /// agent's card.
    Unpinned,
    /// The pinned spool could not be opened; worded for the generation's
    /// degraded notice.
    Failed(String),
}

impl Pinned {
    /// The spool [`crate::inbox_receipt::ENV_INBOX_SPOOL`] names for the
    /// agent `agent_id` in `pane_id`.
    pub fn from_env(pane_id: &str, agent_id: &str) -> Pinned {
        match std::env::var_os(crate::inbox_receipt::ENV_INBOX_SPOOL) {
            None => Pinned::Unpinned,
            Some(path) => match open_pinned(Path::new(&path), pane_id, agent_id) {
                Ok(spool) => Pinned::Open(spool),
                Err(error) => Pinned::Failed(error),
            },
        }
    }
}

/// What [`OneShotBatch::run`] did with a batch.
#[derive(Debug)]
pub struct Ran {
    pub outputs: Vec<Output>,
    /// The write or flush error that stopped the hand-over, if one did.
    pub error: Option<io::Error>,
}

impl Ran {
    /// Whether any byte of any block reached the agent.
    pub fn handed_any(&self) -> bool {
        self.outputs
            .iter()
            .any(|output| *output != Output::NotHanded)
    }
}

#[cfg(unix)]
impl OneShotBatch {
    /// Report every attempt `not_handed` on `connection` without writing a
    /// byte: a one-shot that took the batch but cannot open its output (its
    /// stdout or stderr could not be duplicated) says so, and the deck
    /// requeues the batch at once rather than waiting out its grant.
    pub fn decline(&self, connection: &mut Connection, receiver: Receiver) -> Ran {
        let outputs = vec![Output::NotHanded; self.len()];
        let line = self.controls(&outputs, receiver, None);
        let _ = connection.send(&line, Instant::now() + RECEIPTS_ANSWER_WAIT);
        connection.finish();
        Ran {
            outputs,
            error: None,
        }
    }

    /// The whole one-shot sequence on `connection` (part 2 §1.1, §1.2): ask
    /// `may` for every attempt and read the answers; with nothing awaited
    /// after them, fence each block right before its first byte and write
    /// `pieces` to `out`; publish each attempt's receipt into `pinned`; then
    /// report every attempt on the connection, `degraded` added to the first
    /// publish failure. A spool that is pinned but cannot be opened fails
    /// every fence closed: the deck may have marked a block revoked, and
    /// nothing here can read that.
    #[allow(clippy::too_many_arguments)]
    pub fn run<W: Write>(
        &self,
        connection: &mut Connection,
        pinned: &Pinned,
        grant_ttl: Duration,
        out: &mut W,
        pieces: &[Piece<'_>],
        receiver: Receiver,
        degraded: Option<String>,
    ) -> Ran {
        let approval = connection.approve(self, grant_ttl);
        #[cfg(feature = "e2e")]
        if std::env::var_os(ENV_TEST_EXIT_AFTER_GRANT).is_some() {
            std::process::exit(0);
        }
        let hand = match pinned {
            Pinned::Open(spool) => {
                let mut fence = Fence::new(&approval, spool, connection.arrivals());
                self.hand_over(out, pieces, |at| fence.check(at, Instant::now()))
            }
            Pinned::Unpinned => {
                let mut fence = Fence::unpinned(&approval, connection.arrivals());
                self.hand_over(out, pieces, |at| fence.check(at, Instant::now()))
            }
            Pinned::Failed(_) => HandOver {
                outputs: vec![Output::NotHanded; self.len()],
                error: None,
            },
        };
        let published = match pinned {
            Pinned::Open(spool) => self.publish(Some(spool), &hand.outputs, receiver),
            Pinned::Unpinned => self.publish(None, &hand.outputs, receiver),
            Pinned::Failed(error) => Some(error.clone()),
        };
        let line = self.controls(&hand.outputs, receiver, degraded.or(published));
        let _ = connection.send(&line, Instant::now() + RECEIPTS_ANSWER_WAIT);
        connection.finish();
        Ran {
            outputs: hand.outputs,
            error: hand.error,
        }
    }
}

/// TEST knob, read only by a build with the `e2e` feature: the one-shot
/// integration this process declares instead of
/// [`crate::inbox_hub::HOOK_INTEGRATION`] — a number, or `none` to declare
/// nothing — so a test can stand in for an older dot-agent-deck named in an
/// agent's hook settings.
pub const ENV_TEST_HOOK_INTEGRATION: &str = "DOT_AGENT_DECK_INBOX_TEST_HOOK_INTEGRATION";

/// TEST knob, read only by a build with the `e2e` feature: the one-shot
/// exits as soon as its `may` questions are answered, before it writes a
/// byte, as a hook killed between its grant and its print would.
pub const ENV_TEST_EXIT_AFTER_GRANT: &str = "DOT_AGENT_DECK_INBOX_TEST_EXIT_AFTER_GRANT";

/// The one-shot integration this process declares (part 2 §2.7). A process
/// that declares none stands in for an older dot-agent-deck, and speaks the
/// legacy verbs as one would: it asks no `inbox_receipts` question and sends
/// `inbox_wait` or `inbox_fetch`. Only [`ENV_TEST_HOOK_INTEGRATION`] makes it
/// `None`.
pub fn declared_hook_integration() -> Option<u32> {
    #[cfg(feature = "e2e")]
    if let Ok(raw) = std::env::var(ENV_TEST_HOOK_INTEGRATION) {
        return raw.trim().parse().ok();
    }
    Some(crate::inbox_hub::HOOK_INTEGRATION)
}

/// The socket-wait budget of a one-shot's receipts question: the connect's
/// wait for the deck to accept, writing the question and waiting for the
/// answer ([`Connection::open_receipts_within`]). A deck that has not answered
/// by then is sent nothing that could take mail ([`Opened::TimedOut`]). An
/// older daemon answers nothing at all (it cannot decode the question), so
/// this is what that pairing costs each question. It is a cooperative budget,
/// checked around each socket wait, not a ceiling on the call: resolving the
/// socket's path inside `connect(2)` is not pre-empted by it. The waiter asks
/// once within it; Claude's prompt hook shares one such budget between its
/// commit and its doorbell fetch
/// ([`crate::inbox_prompt::prompt_receipts_wait`]).
pub const RECEIPTS_ANSWER_WAIT: Duration = Duration::from_secs(5);

/// TEST knob, read only by a build with the `e2e` feature:
/// [`RECEIPTS_ANSWER_WAIT`] in milliseconds, so a test of an older daemon
/// does not wait the whole of it.
pub const ENV_TEST_RECEIPTS_WAIT_MS: &str = "DOT_AGENT_DECK_INBOX_TEST_RECEIPTS_WAIT_MS";

/// [`RECEIPTS_ANSWER_WAIT`], or what [`ENV_TEST_RECEIPTS_WAIT_MS`] sets.
pub fn receipts_answer_wait() -> Duration {
    #[cfg(feature = "e2e")]
    if let Some(ms) = std::env::var(ENV_TEST_RECEIPTS_WAIT_MS)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
    {
        return Duration::from_millis(ms);
    }
    RECEIPTS_ANSWER_WAIT
}

/// What a one-shot says when the deck answered without confirming that it
/// serves receipts ([`Opened::Unsupported`]): nothing was taken, and how to
/// make the two agree. `hooks install` writes the waiter and the prompt hook
/// together, from the one binary it runs as. A deck that did not answer in
/// time gets [`RECEIPTS_TIMED_OUT`] instead, and a connection lost before
/// any answer [`RECEIPTS_CONNECTION_LOST`]: neither is an answer, so neither
/// is evidence of an older deck.
pub const RECEIPTS_UNSUPPORTED: &str = "dot-agent-deck: the deck daemon did not confirm that it serves message receipts, so no message was taken. It is most likely older than the dot-agent-deck this agent's hooks run. Restart the deck daemon on that newer build, or run `dot-agent-deck hooks install --agent claude-code` with the build the daemon runs, which rewrites the waiter and the prompt hook together; then restart the agent.";

static REPORTED_UNSUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_UNSUPPORTED`] on stderr, once per process, and remember it,
/// so the hook exits with the status that shows its stderr to the user
/// ([`reported_unsupported`]).
pub fn report_unsupported() {
    if !REPORTED_UNSUPPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_UNSUPPORTED}");
    }
}

/// Whether this process said [`RECEIPTS_UNSUPPORTED`].
pub fn reported_unsupported() -> bool {
    REPORTED_UNSUPPORTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// What a one-shot that would have taken mail says when its receipts budget
/// ran out before the deck answered whether it serves receipts, or before
/// the phase could ask ([`Opened::TimedOut`]): nothing was asked that could
/// take mail, so the messages stay queued. Fixed text, and not
/// [`RECEIPTS_UNSUPPORTED`]'s: a slow deck that serves receipts and an older
/// one, which answers nothing, look the same from here, so it names both
/// rather than the likelier. A commit that runs out of time says
/// [`RECEIPTS_COMMIT_TIMED_OUT`] instead, which promises nothing queued.
pub const RECEIPTS_TIMED_OUT: &str = "dot-agent-deck: the deck daemon did not answer in time whether it serves message receipts, so this hook took no message and left the messages waiting for this agent queued. A busy deck daemon can answer late; one older than the dot-agent-deck this agent's hooks run does not answer at all. If this keeps happening, make the two the same build: restart the deck daemon on the hooks' build, or run `dot-agent-deck hooks install --agent claude-code` with the build the daemon runs, which rewrites the waiter and the prompt hook together; then restart the agent.";

static REPORTED_TIMED_OUT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_TIMED_OUT`] on stderr, once per process, and remember it,
/// so the hook exits with the status that shows its stderr to the user
/// ([`reported_refusal`]).
pub fn report_timed_out() {
    if !REPORTED_TIMED_OUT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_TIMED_OUT}");
    }
}

/// Whether this process said [`RECEIPTS_TIMED_OUT`].
pub fn reported_timed_out() -> bool {
    REPORTED_TIMED_OUT.load(std::sync::atomic::Ordering::SeqCst)
}

/// What the prompt hook says when its commit's receipts budget ran out before
/// the deck answered whether it serves receipts ([`Opened::TimedOut`] on the
/// commit's connection). The message the prompt carries was already taken by
/// the waiter that wrote its block, so this says only what this exchange did
/// not do: it confirmed no receipt and took no further message. It makes no
/// claim that anything is still queued.
pub const RECEIPTS_COMMIT_TIMED_OUT: &str = "dot-agent-deck: the deck daemon did not answer in time whether it serves message receipts, so this hook did not confirm to it that the message in this prompt was received, and took no further message. A busy deck daemon can answer late; one older than the dot-agent-deck this agent's hooks run does not answer at all. If this keeps happening, make the two the same build: restart the deck daemon on the hooks' build, or run `dot-agent-deck hooks install --agent claude-code` with the build the daemon runs, which rewrites the waiter and the prompt hook together; then restart the agent.";

static REPORTED_COMMIT_TIMED_OUT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_COMMIT_TIMED_OUT`] on stderr, once per process, and
/// remember it ([`reported_refusal`]).
pub fn report_commit_timed_out() {
    if !REPORTED_COMMIT_TIMED_OUT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_COMMIT_TIMED_OUT}");
    }
}

/// Whether this process said [`RECEIPTS_COMMIT_TIMED_OUT`].
pub fn reported_commit_timed_out() -> bool {
    REPORTED_COMMIT_TIMED_OUT.load(std::sync::atomic::Ordering::SeqCst)
}

/// What a one-shot says when its connection to the deck was lost before the
/// deck answered whether it serves receipts ([`Opened::Lost`]): the
/// connection ended, the question's write failed for another reason than
/// running out of time, or a line could not be read. No answer came, so this
/// is no evidence of the deck's version, and it names none: it says what this
/// exchange did not do and how to check the deck is running.
pub const RECEIPTS_CONNECTION_LOST: &str = "dot-agent-deck: the connection to the deck daemon was lost before it answered whether it serves message receipts, so this hook took no message and confirmed no receipt. The deck daemon may have stopped or restarted: check that it is running with `dot-agent-deck daemon status`, and if this keeps happening while it runs, restart the agent.";

static REPORTED_CONNECTION_LOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_CONNECTION_LOST`] on stderr, once per process, and remember
/// it ([`reported_refusal`]).
pub fn report_connection_lost() {
    if !REPORTED_CONNECTION_LOST.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_CONNECTION_LOST}");
    }
}

/// Whether this process said [`RECEIPTS_CONNECTION_LOST`].
pub fn reported_connection_lost() -> bool {
    REPORTED_CONNECTION_LOST.load(std::sync::atomic::Ordering::SeqCst)
}

/// What the prompt hook says when the deck answered its commit's question
/// without confirming that it serves receipts ([`Opened::Unsupported`] on the
/// commit's connection). [`RECEIPTS_UNSUPPORTED`]'s "no message was taken" is
/// false here: the waiter already took the message the prompt carries. So
/// this says only what this exchange did not do, and how to make the two
/// builds agree.
pub const RECEIPTS_COMMIT_UNSUPPORTED: &str = "dot-agent-deck: the deck daemon did not confirm that it serves message receipts, so this hook did not confirm to it that the message in this prompt was received, and took no further message. The deck daemon is most likely older than the dot-agent-deck this agent's hooks run. Restart the deck daemon on that newer build, or run `dot-agent-deck hooks install --agent claude-code` with the build the daemon runs, which rewrites the waiter and the prompt hook together; then restart the agent.";

static REPORTED_COMMIT_UNSUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_COMMIT_UNSUPPORTED`] on stderr, once per process, and
/// remember it ([`reported_refusal`]).
pub fn report_commit_unsupported() {
    if !REPORTED_COMMIT_UNSUPPORTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_COMMIT_UNSUPPORTED}");
    }
}

/// Whether this process said [`RECEIPTS_COMMIT_UNSUPPORTED`].
pub fn reported_commit_unsupported() -> bool {
    REPORTED_COMMIT_UNSUPPORTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// What the prompt hook says when its commit's connection was lost before the
/// deck answered whether it serves receipts ([`Opened::Lost`] on the commit's
/// connection). Like [`RECEIPTS_COMMIT_TIMED_OUT`], it does not say no message
/// was taken: the prompt carries one the waiter took.
pub const RECEIPTS_COMMIT_CONNECTION_LOST: &str = "dot-agent-deck: the connection to the deck daemon was lost before it answered whether it serves message receipts, so this hook did not confirm to it that the message in this prompt was received, and took no further message. The deck daemon may have stopped or restarted: check that it is running with `dot-agent-deck daemon status`, and if this keeps happening while it runs, restart the agent.";

static REPORTED_COMMIT_CONNECTION_LOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_COMMIT_CONNECTION_LOST`] on stderr, once per process, and
/// remember it ([`reported_refusal`]).
pub fn report_commit_connection_lost() {
    if !REPORTED_COMMIT_CONNECTION_LOST.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_COMMIT_CONNECTION_LOST}");
    }
}

/// Whether this process said [`RECEIPTS_COMMIT_CONNECTION_LOST`].
pub fn reported_commit_connection_lost() -> bool {
    REPORTED_COMMIT_CONNECTION_LOST.load(std::sync::atomic::Ordering::SeqCst)
}

/// Forget what this process said, so a test can tell which notice a later
/// step says.
#[cfg(all(test, unix))]
pub(crate) fn forget_reports() {
    for said in [
        &REPORTED_UNSUPPORTED,
        &REPORTED_TIMED_OUT,
        &REPORTED_COMMIT_TIMED_OUT,
        &REPORTED_CONNECTION_LOST,
        &REPORTED_COMMIT_UNSUPPORTED,
        &REPORTED_COMMIT_CONNECTION_LOST,
        &REPORTED_FOREIGN,
    ] {
        said.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a one-shot says when the deck socket it connects to refused it access
/// ([`Opened::Foreign`]): it asked nothing there, and the remedy for each of
/// the two causes the refusal cannot tell apart, a deck listening there as
/// another user and permissions that deny this user (the socket's own mode,
/// or a directory above it). Fixed text: the socket's path and the uids are
/// not in it.
pub const RECEIPTS_FOREIGN_SOCKET: &str = "dot-agent-deck: the deck socket refused this hook access, either because the deck listening on it runs as another user or because of the permissions of the socket or a directory above it, so the hook asked it nothing and no message was taken. If you set DOT_AGENT_DECK_SOCKET or DOT_AGENT_DECK_ATTACH_SOCKET, point them at your own deck daemon's sockets, or unset them to use the default; if they already point at your own deck, or you set neither, fix the permissions of that socket and the directories above it, the default socket included, so that you can connect to it; then restart the agent.";

static REPORTED_FOREIGN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Say [`RECEIPTS_FOREIGN_SOCKET`] on stderr, once per process, and remember
/// it, so the hook exits with the status that shows its stderr to the user
/// ([`reported_refusal`]).
pub fn report_foreign_socket() {
    if !REPORTED_FOREIGN.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{RECEIPTS_FOREIGN_SOCKET}");
    }
}

/// Whether this process said [`RECEIPTS_UNSUPPORTED`], [`RECEIPTS_TIMED_OUT`],
/// [`RECEIPTS_COMMIT_TIMED_OUT`], [`RECEIPTS_CONNECTION_LOST`],
/// [`RECEIPTS_COMMIT_UNSUPPORTED`], [`RECEIPTS_COMMIT_CONNECTION_LOST`] or
/// [`RECEIPTS_FOREIGN_SOCKET`]: the phase that said one took no further
/// mail (a commit's prompt already carries the message the waiter took), and
/// a later phase of the same hook asks nothing more.
pub fn reported_refusal() -> bool {
    reported_unsupported()
        || reported_timed_out()
        || reported_commit_timed_out()
        || reported_connection_lost()
        || reported_commit_unsupported()
        || reported_commit_connection_lost()
        || REPORTED_FOREIGN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether `reply`, the deck's answer to an `inbox_receipts` question, says
/// it serves the one-shot receipts this binary speaks.
pub fn serves_receipts(reply: &InboxWaitReply) -> bool {
    reply
        .one_shot_receipts
        .is_some_and(|served| served >= crate::inbox_hub::ONE_SHOT_RECEIPTS)
}

/// The `inbox_receipts` question for the agent `agent_id` in `pane_id`.
pub fn receipts_question(pane_id: &str, agent_id: Option<&str>, token: &str) -> Option<String> {
    serde_json::to_string(&crate::event::DaemonMessage::InboxReceipts(
        crate::event::InboxReceiptsRequest {
            pane_id: pane_id.to_string(),
            agent_id: agent_id.map(str::to_string),
            token: Some(token.to_string()),
        },
    ))
    .ok()
}

/// What the deck said when a one-shot asked whether it serves receipts
/// ([`Connection::open_receipts`]).
#[cfg(unix)]
#[derive(Debug)]
pub enum Opened {
    /// It serves them: the connection is ready for the one-shot's request.
    Ready(Connection),
    /// It answered with a refusal (of the token, the pane or the agent): a
    /// deck that serves receipts, and not to this caller.
    Refused(Box<InboxWaitReply>),
    /// It answered without saying it serves them (the answer had no
    /// [`crate::inbox_hub::ONE_SHOT_RECEIPTS`]: a legacy batch, or a
    /// capability below this binary's). Nothing that could take mail or
    /// commit it was sent.
    Unsupported,
    /// The connection was lost before an answer: it ended, a line could not
    /// be read, or the question's write failed for another reason than
    /// running out of time. No answer is no evidence of the deck's version.
    /// Nothing that could take mail or commit it was sent.
    Lost,
    /// The budget ran out first: it was already spent, or the connect, the
    /// question's write or the wait for the answer did not finish by the
    /// deadline (a full accept queue, a deck that does not read, a slow deck,
    /// an older one, which answers nothing). Nothing that could take mail or
    /// commit it was sent.
    TimedOut,
    /// The socket is one this user may not use: its listener runs as another
    /// user, or connecting to it was denied (the socket's mode, or a
    /// directory above it). Refused before the question, which carries the
    /// token, was written.
    Foreign,
    /// The connect failed for another reason than running out of time or
    /// [`Opened::Foreign`]: no socket, or nothing listening on it.
    Unreachable,
}

/// The most of one line from the daemon a one-shot reads.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// The most lines a connection's reader thread queues that the one-shot has
/// not yet taken, each of at most [`MAX_LINE_BYTES`]. With the queue full the
/// thread holds one more, parsed, while it waits for room, and stops reading,
/// so the deck's further lines wait in the socket and then in the deck rather
/// than in this process.
pub const MAX_QUEUED_LINES: usize = 8;

/// A one-shot's side of its one connection to the deck (part 2 §2.5). A
/// reader thread of its own reads every line through [`read_frame`] and
/// hands it over parsed, queueing at most [`MAX_QUEUED_LINES`] (and holding a
/// further one while it waits for room); an end of the
/// connection, a line cut short by it, a line past [`MAX_LINE_BYTES`] or a
/// read error ends the connection's use for good, and nothing after it is
/// read. So [`Connection::arrivals`] is a non-blocking [`Incoming`] that
/// never splits a line. A write through [`Connection::send`] is given the
/// time left before its deadline.
#[cfg(unix)]
#[derive(Debug)]
pub struct Connection {
    stream: std::os::unix::net::UnixStream,
    lines: std::sync::mpsc::Receiver<Option<InboxWaitReply>>,
    /// Lines read before the hand-over that its fence must still see.
    held: Vec<InboxWaitReply>,
    /// A line received past the deadline it was waited for: not returned
    /// then, and first in line for whatever reads next.
    unread: Option<Box<InboxWaitReply>>,
    ended: bool,
    next_q: u64,
    /// A line [`Connection::send`] gave up on partway: the next line sent
    /// starts with the newline that ends it, so the deck discards the cut
    /// line and reads the next one whole.
    cut: bool,
}

#[cfg(unix)]
impl Connection {
    /// Connect to the deck at `path` and send `request`, one line, with no
    /// bound on the connect or the write, as an older dot-agent-deck does.
    /// Only a process standing in for one ([`declared_hook_integration`]
    /// `None`) opens a connection this way; every other one-shot asks first
    /// ([`Connection::open_receipts`]).
    pub fn open(path: &Path, request: &str) -> io::Result<Connection> {
        let mut stream = std::os::unix::net::UnixStream::connect(path)?;
        stream.write_all(format!("{request}\n").as_bytes())?;
        stream.flush()?;
        Connection::over(stream, MAX_LINE_BYTES)
    }

    /// PRD #1666 part 2 (wire audit W03–W05): connect to the deck at `path`,
    /// ask `question` (an `inbox_receipts` line, [`receipts_question`]) and
    /// read the answer, the three of them within one socket-wait budget of
    /// [`RECEIPTS_ANSWER_WAIT`]. Every one-shot that could take mail or
    /// commit it asks first, on the connection that then carries its
    /// request, and decides from this answer alone, never from an earlier
    /// one: the deck that answers is the one the request reaches. An older
    /// daemon cannot decode the question and answers nothing, so it is never
    /// sent the request.
    pub fn open_receipts(path: &Path, question: &str) -> Opened {
        Connection::open_receipts_within(path, question, receipts_answer_wait())
    }

    /// [`Connection::open_receipts`] within `wait`, from now.
    pub fn open_receipts_within(path: &Path, question: &str, wait: Duration) -> Opened {
        Connection::open_receipts_by(path, question, Instant::now() + wait)
    }

    /// [`Connection::open_receipts`] by `deadline`, a socket-wait budget
    /// checked around each wait: a deadline already spent, a connect parked
    /// on a full accept queue, a question the deck does not read, and an
    /// answer received after it, even queued behind keepalives that came in
    /// time, are all [`Opened::TimedOut`]. It cannot pre-empt `connect(2)`
    /// resolving the socket's path (a stalled filesystem); once that returns,
    /// a deadline already past sends nothing.
    pub fn open_receipts_by(path: &Path, question: &str, deadline: Instant) -> Opened {
        let wait = deadline.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            return Opened::TimedOut;
        }
        let stream = match crate::platform::ipc::IpcClient::connect_timeout(path, wait) {
            Ok(client) => client.into_std(),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => return Opened::TimedOut,
            // A listener running as another user, or a socket this user may
            // not connect to (its mode, or a directory above it): refused
            // before the question, which carries the token, is written.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Opened::Foreign;
            }
            Err(_) => return Opened::Unreachable,
        };
        let Ok(mut connection) = Connection::over(stream, MAX_LINE_BYTES) else {
            return Opened::Unreachable;
        };
        if let Err(error) = connection.send(question, deadline) {
            return if error.kind() == io::ErrorKind::TimedOut {
                Opened::TimedOut
            } else {
                Opened::Lost
            };
        }
        loop {
            let Some(reply) = connection.next(Some(deadline)) else {
                // The connection ended unanswered, or the deadline passed.
                return if connection.ended() {
                    Opened::Lost
                } else {
                    Opened::TimedOut
                };
            };
            if reply.keepalive {
                continue;
            }
            return if reply.refused.is_some() {
                Opened::Refused(Box::new(reply))
            } else if serves_receipts(&reply) {
                Opened::Ready(connection)
            } else {
                Opened::Unsupported
            };
        }
    }

    /// The connection on `stream`, reading lines of at most `max` bytes.
    fn over(stream: std::os::unix::net::UnixStream, max: usize) -> io::Result<Connection> {
        let reading = stream.try_clone()?;
        let (tx, lines) = std::sync::mpsc::sync_channel(MAX_QUEUED_LINES);
        std::thread::spawn(move || {
            let mut reader = io::BufReader::new(reading);
            loop {
                match read_frame(&mut reader, max) {
                    Frame::Line(line) => {
                        // A line that is not a reply is skipped; it never
                        // ends the connection.
                        if let Ok(reply) = serde_json::from_slice::<InboxWaitReply>(&line)
                            && tx.send(Some(reply)).is_err()
                        {
                            return;
                        }
                    }
                    Frame::Eof | Frame::Unterminated | Frame::TooLong | Frame::Failed(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                }
            }
        });
        Ok(Connection {
            stream,
            lines,
            held: Vec::new(),
            unread: None,
            ended: false,
            next_q: 1,
            cut: false,
        })
    }

    /// The next line, waiting until `deadline` (forever with `None`).
    /// `None` once the connection has ended, or once the deadline has passed,
    /// even with lines already waiting: a deck that keeps lines coming cannot
    /// stretch it. The clock is read again after a line is received, so one
    /// that came in, or was taken, after the deadline is not returned for it
    /// (R3AR-R01); it stays first in line for the fence or a later read.
    pub fn next(&mut self, deadline: Option<Instant>) -> Option<InboxWaitReply> {
        self.next_by(deadline, &mut Instant::now)
    }

    /// [`Connection::next`] reading the clock from `now`.
    fn next_by(
        &mut self,
        deadline: Option<Instant>,
        now: &mut dyn FnMut() -> Instant,
    ) -> Option<InboxWaitReply> {
        use std::sync::mpsc::RecvTimeoutError;
        let expired = |now: &mut dyn FnMut() -> Instant| deadline.is_some_and(|d| now() >= d);
        if let Some(reply) = self.unread.take() {
            if expired(now) {
                self.unread = Some(reply);
                return None;
            }
            return Some(*reply);
        }
        if self.ended {
            return None;
        }
        let got = match deadline {
            None => self.lines.recv().unwrap_or(None),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(now());
                if left.is_zero() {
                    return None;
                }
                match self.lines.recv_timeout(left) {
                    Ok(got) => got,
                    Err(RecvTimeoutError::Timeout) => return None,
                    Err(RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        let Some(reply) = got else {
            self.ended = true;
            return None;
        };
        if expired(now) {
            self.unread = Some(Box::new(reply));
            return None;
        }
        Some(reply)
    }

    /// Keep `reply` for the fence: a revoke or a terminal line read before
    /// the hand-over.
    pub fn hold(&mut self, reply: InboxWaitReply) {
        self.held.push(reply);
    }

    /// Whether the connection has ended.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Send one line within the time left before `deadline`, or fail with
    /// [`io::ErrorKind::TimedOut`] (a deck that does not read).
    /// The clock is checked before each piece is written, and each write is
    /// given the time that is left as its timeout; a line given up on is not
    /// continued later, and the next line sent first ends it.
    pub fn send(&mut self, line: &str, deadline: Instant) -> io::Result<()> {
        let line = format!("{}{line}\n", if self.cut { "\n" } else { "" });
        let mut rest = line.as_bytes();
        let sent = loop {
            if rest.is_empty() {
                break Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Err(io::Error::from(io::ErrorKind::TimedOut));
            }
            if let Err(error) = self.stream.set_write_timeout(Some(left)) {
                break Err(error);
            }
            match self.stream.write(rest) {
                Ok(0) => break Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => rest = &rest[n..],
                // The write timed out, or was interrupted: the deadline
                // decides whether to try again.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => break Err(error),
            }
        };
        let _ = self.stream.set_write_timeout(None);
        if sent.is_ok() {
            self.cut = false;
        } else if rest.len() < line.len() {
            self.cut = true;
        }
        sent
    }

    /// Ask `may` for every attempt of `batch`, recording when first, and
    /// read the answers until each question is answered, the approval runs
    /// out or the connection ends. Whatever else arrives meanwhile is kept
    /// for the fence.
    pub fn approve(&mut self, batch: &OneShotBatch, grant_ttl: Duration) -> Approval {
        let sent_at = Instant::now();
        let (mut approval, lines) = batch.ask(self.next_q, sent_at, grant_ttl);
        self.next_q += lines.len() as u64;
        let mut unanswered = lines.len();
        for line in &lines {
            if self.send(line, approval.valid_until()).is_err() {
                return approval;
            }
        }
        while unanswered > 0 {
            let Some(reply) = self.next(Some(approval.valid_until())) else {
                break;
            };
            if reply.keepalive {
                continue;
            }
            if let Some(grant) = &reply.grant {
                approval.answer(grant, Instant::now());
                unanswered = unanswered.saturating_sub(1);
            }
            if !reply.revoke.is_empty()
                || reply.superseded
                || reply.not_in_deck
                || reply.refused.is_some()
            {
                self.hold(reply);
            }
        }
        approval
    }

    /// What has arrived and can be read without waiting, for the fence.
    pub fn arrivals(&mut self) -> Arrivals<'_> {
        Arrivals(self)
    }

    /// Done writing: the deck reads every line sent before this.
    pub fn finish(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }
}

/// A connection given up shuts its socket, so its reader thread ends and the
/// deck sees it close rather than wait out its idle window.
#[cfg(unix)]
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// See [`Connection::arrivals`].
#[cfg(unix)]
pub struct Arrivals<'a>(&'a mut Connection);

#[cfg(unix)]
impl Incoming for Arrivals<'_> {
    fn arrived(&mut self) -> Arrived {
        let connection = &mut *self.0;
        let mut lines = std::mem::take(&mut connection.held);
        lines.extend(connection.unread.take().map(|reply| *reply));
        while !connection.ended {
            match connection.lines.try_recv() {
                Ok(Some(reply)) => lines.push(reply),
                Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    connection.ended = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        Arrived {
            lines,
            eof: connection.ended,
        }
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
pub(crate) mod tests {
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
            FenceRefusal::Superseded,
            FenceRefusal::Refused("not_in_deck".to_string()),
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

    /// Scenario: Hand over a batch whose first block is written in two
    /// pieces, as a hook's escaped output may write it: the prefix is
    /// accepted and the suffix's first write fails; the suffix is accepted
    /// only in part; a fence that would refuse once the block is under way;
    /// and a block whose first piece is empty, so its first byte is in the
    /// second piece, where the fence refuses it. A block with any byte
    /// accepted is never not handed, one is written only when every piece
    /// went in full, and the fence runs once, before the block's first
    /// byte.
    #[spec("inbox/receipt/026")]
    #[test]
    fn receipt_026_a_block_in_several_pieces_is_judged_as_one() {
        use Output::{NotHanded, Possible, Written};
        let batch = two_blocks();
        let text = batch.text(0).unwrap().as_bytes();
        let (prefix, suffix) = text.split_at(10);
        let pieces = [
            Piece::Block(0, prefix),
            Piece::Block(0, suffix),
            Piece::Frame(SEP.as_bytes()),
            Piece::Block(1, batch.text(1).unwrap().as_bytes()),
        ];
        for (what, steps, expected) in [
            (
                "the suffix's first write fails",
                vec![
                    Step::Take(prefix.len()),
                    Step::Fail(io::ErrorKind::BrokenPipe),
                ],
                vec![Possible, NotHanded],
            ),
            (
                "the suffix is accepted in part",
                vec![
                    Step::Take(prefix.len()),
                    Step::Take(3),
                    Step::Fail(io::ErrorKind::BrokenPipe),
                ],
                vec![Possible, NotHanded],
            ),
            ("every piece in full", vec![], vec![Written, Written]),
        ] {
            let mut out = pipe(steps);
            let handed = batch.hand_over(&mut out, &pieces, |_| Ok(()));
            assert_eq!(handed.outputs, expected, "{what}");
        }

        let mut fenced = Vec::new();
        let mut out = pipe(vec![]);
        let handed = batch.hand_over(&mut out, &pieces, |at| {
            fenced.push(at.clone());
            if fenced.len() > 1 && at.id == ID1 {
                Err(FenceRefusal::Revoked)
            } else {
                Ok(())
            }
        });
        assert_eq!(
            fenced,
            vec![at(ID1, 1, 1), at(ID2, 1, 1)],
            "one fence per block, before its first byte"
        );
        assert_eq!(handed.outputs, vec![Written, Written]);
        assert!(
            out.written.starts_with(text),
            "a block under way is not cut short"
        );

        let empty_first = [
            Piece::Block(0, b""),
            Piece::Block(0, text),
            Piece::Frame(SEP.as_bytes()),
            Piece::Block(1, batch.text(1).unwrap().as_bytes()),
        ];
        let mut out = pipe(vec![]);
        let handed = batch.hand_over(&mut out, &empty_first, |at| {
            if at.id == ID1 {
                Err(FenceRefusal::Revoked)
            } else {
                Ok(())
            }
        });
        assert_eq!(handed.outputs, vec![NotHanded, Written]);
        assert!(
            !String::from_utf8(out.written).unwrap().contains(ID1),
            "refused at its first byte, in its second piece"
        );
    }

    /// Scenario: Approve a two-block batch, then let the deck's
    /// `superseded` line arrive with the connection still open and no revoke
    /// marker, and separately a `not_in_deck` line and a refusal line. The
    /// fence refuses block one before its first byte and keeps refusing
    /// block two when nothing more arrives, so a hand-over writes none of
    /// either block.
    #[spec("inbox/receipt/027")]
    #[test]
    fn receipt_027_a_terminal_line_already_arrived_ends_the_hand_over() {
        let deck = deck();
        let batch = two_blocks();
        let [one, two] = <[InboxAttemptRef; 2]>::try_from(batch.attempts()).unwrap();
        let t = Instant::now();
        let approval = approve(&batch, t, t);
        for (what, line, refusal) in [
            (
                "superseded",
                InboxWaitReply::superseded(),
                FenceRefusal::Superseded,
            ),
            (
                "not in deck",
                InboxWaitReply::not_in_deck(),
                FenceRefusal::Refused("not_in_deck".to_string()),
            ),
            (
                "refused",
                InboxWaitReply::refused("unattested", "the caller is not attested"),
                FenceRefusal::Refused("unattested".to_string()),
            ),
        ] {
            let arrived = || Arrived {
                lines: vec![line.clone()],
                eof: false,
            };
            let mut fence = Fence::new(&approval, &deck.component, Script(vec![arrived()].into()));
            assert_eq!(fence.check(&one, t), Err(refusal.clone()), "{what}");
            assert_eq!(fence.check(&two, t), Err(refusal), "{what}: it stays ended");

            let mut out = pipe(vec![]);
            let mut fence = Fence::new(&approval, &deck.component, Script(vec![arrived()].into()));
            let handed = batch.hand_over(&mut out, &batch.plain_pieces("", SEP), |at| {
                fence.check(at, t)
            });
            assert_eq!(
                handed.outputs,
                vec![Output::NotHanded, Output::NotHanded],
                "{what}"
            );
            let written = String::from_utf8(out.written).unwrap();
            assert!(
                !written.contains(ID1) && !written.contains(ID2),
                "{what}: no byte of either block: {written:?}"
            );
        }
    }

    /// Scenario: A one-shot's connection is handed a reply split across two writes, a line that is not a reply, a revoke, then a line longer than its bound and a reply after it. It hands the split reply over whole, skips the stray line, and gives its fence the revoke without waiting; the overlong line ends the connection's use, so the reply after it is never read, and every later look sees it ended. On another connection, a reply cut off by the end of the connection is never handed over.
    #[cfg(unix)]
    #[spec("inbox/receipt/029")]
    #[test]
    fn receipt_029_a_one_shots_connection_hands_over_whole_lines_and_ends_for_good() {
        use std::os::unix::net::UnixStream;
        let (ours, mut deck) = UnixStream::pair().unwrap();
        let mut connection = Connection::over(ours, 256).unwrap();
        deck.write_all(b"{\"keepalive\":").unwrap();
        deck.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        deck.write_all(b"true}\nnot a reply\n").unwrap();
        let soon = || Some(Instant::now() + Duration::from_secs(5));
        assert!(
            connection
                .next(soon())
                .expect("the split line, whole")
                .keepalive
        );
        let revoke = format!("{{\"revoke\":[{{\"id\":\"{ID1}\",\"a\":1,\"e\":1}}]}}\n");
        deck.write_all(revoke.as_bytes()).unwrap();
        let mut arrived = Arrived::default();
        let started = Instant::now();
        while arrived.lines.is_empty() {
            arrived = connection.arrivals().arrived();
            assert!(started.elapsed() < Duration::from_secs(5));
        }
        assert_eq!(arrived.lines[0].revoke, vec![at(ID1, 1, 1)]);
        assert!(!arrived.eof);
        let polled = Instant::now();
        assert!(connection.arrivals().arrived().lines.is_empty());
        assert!(
            polled.elapsed() < Duration::from_millis(100),
            "a look never waits"
        );
        deck.write_all(&[b'x'; 300]).unwrap();
        deck.write_all(b"\n{\"superseded\":true}\n").unwrap();
        assert!(
            connection.next(soon()).is_none(),
            "an overlong line ends it"
        );
        assert!(connection.ended());
        let after = connection.arrivals().arrived();
        assert!(
            after.eof && after.lines.is_empty(),
            "nothing after it is read"
        );

        let (ours, mut deck) = UnixStream::pair().unwrap();
        let mut cut = Connection::over(ours, 256).unwrap();
        deck.write_all(b"{\"superseded\":true}").unwrap();
        drop(deck);
        assert!(
            cut.next(soon()).is_none(),
            "a cut line is never handed over"
        );
        assert!(cut.ended());
    }

    /// A one-shot's connection and the deck's end of it.
    #[cfg(unix)]
    pub(crate) fn connection_pair() -> (Connection, std::os::unix::net::UnixStream) {
        let (ours, deck) = std::os::unix::net::UnixStream::pair().unwrap();
        (Connection::over(ours, MAX_LINE_BYTES).unwrap(), deck)
    }

    /// Every control line the deck reads on `deck` until the one-shot closes
    /// its side, as the daemon parses them: a line that is not a control is
    /// skipped.
    #[cfg(unix)]
    pub(crate) fn controls_read(deck: std::os::unix::net::UnixStream) -> Vec<InboxStreamControl> {
        use std::io::BufRead;
        io::BufReader::new(deck)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    /// Qodo on PR #1817 (finding 18): a one-shot that took a batch and then
    /// cannot open its output reports every attempt not handed, so the deck
    /// requeues them at once rather than waiting out their grant.
    #[cfg(unix)]
    #[test]
    fn a_batch_with_nowhere_to_go_is_reported_not_handed() {
        for receiver in [Receiver::Hook, Receiver::Waiter] {
            let batch = two_blocks();
            let (mut connection, deck) = connection_pair();
            let ran = batch.decline(&mut connection, receiver);
            assert!(!ran.handed_any(), "{receiver:?}");
            drop(connection);
            let controls = controls_read(deck);
            assert_eq!(controls.len(), 1, "{receiver:?}: {controls:?}");
            assert_eq!(controls[0].not_handed, batch.attempts(), "{receiver:?}");
            assert!(controls[0].handed.is_empty() && controls[0].committed.is_empty());
        }
    }

    /// Qodo on PR #1817 (finding 19): a line given up on partway, as a deck
    /// that stopped reading leaves one, is ended before the next line is
    /// sent, so the deck reads that next line (the one-shot's report) whole
    /// rather than appended to the cut one.
    #[cfg(unix)]
    #[test]
    fn a_line_after_a_cut_one_is_read_whole() {
        let (mut connection, mut deck) = connection_pair();
        let long = format!("{{\"may\":\"{}\"}}", "x".repeat(4 << 20));
        let cut = connection.send(&long, Instant::now() + Duration::from_millis(200));
        assert_eq!(
            cut.map_err(|error| error.kind()),
            Err(io::ErrorKind::TimedOut),
            "precondition: the deck reads nothing, so the long line is cut"
        );
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            io::Read::read_to_end(&mut deck, &mut bytes).unwrap();
            bytes
        });
        let report = control_line(&InboxStreamControl {
            not_handed: vec![at(ID1, 1, 1)],
            ..InboxStreamControl::default()
        });
        connection
            .send(&report, Instant::now() + Duration::from_secs(5))
            .expect("the report is sent");
        connection.finish();
        let bytes = reader.join().unwrap();
        let text = String::from_utf8_lossy(&bytes);
        let controls: Vec<InboxStreamControl> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        assert_eq!(
            controls
                .iter()
                .map(|c| c.not_handed.clone())
                .collect::<Vec<_>>(),
            vec![vec![at(ID1, 1, 1)]],
            "the report is read as a line of its own"
        );
    }

    /// Scenario: Read the deck's once line through the one-shot's reader:
    /// a whole line, a complete JSON reply whose newline never came before
    /// the connection ended (also delivered in two reads), an empty
    /// connection and an overlong line. Only the whole line is handed back;
    /// the reply without its newline is dropped unparsed, although
    /// `read_line` would have handed it back and it parses. The daemon's
    /// classification of a write that lost that newline matches: not
    /// handed.
    #[spec("inbox/receipt/028")]
    #[test]
    fn receipt_028_a_line_without_its_newline_is_never_parsed() {
        use std::io::{BufRead, BufReader, Read};
        let reply = InboxWaitReply::messages(vec![handout(ID1, 1, 1, None)]);
        let json = serde_json::to_string(&reply).unwrap();
        let frame = format!("{json}\n");
        let max = frame.len();

        match read_frame(&mut frame.as_bytes(), max) {
            Frame::Line(line) => {
                serde_json::from_slice::<InboxWaitReply>(&line).unwrap();
            }
            other => panic!("a whole line: {other:?}"),
        }

        let cut = &frame.as_bytes()[..frame.len() - 1];
        let mut legacy = String::new();
        let mut legacy_reader = cut;
        legacy_reader.read_line(&mut legacy).unwrap();
        assert!(
            serde_json::from_str::<InboxWaitReply>(&legacy).is_ok(),
            "precondition: read_line hands it back and it parses"
        );
        assert!(matches!(read_frame(&mut &*cut, max), Frame::Unterminated));
        let (first, second) = cut.split_at(cut.len() / 2);
        let mut two_reads = BufReader::with_capacity(8, first.chain(second));
        assert!(matches!(
            read_frame(&mut two_reads, max),
            Frame::Unterminated
        ));
        assert!(matches!(read_frame(&mut &b""[..], max), Frame::Eof));
        assert!(matches!(
            read_frame(&mut frame.as_bytes(), json.len() - 1),
            Frame::TooLong
        ));
        let mut two = format!("{frame}{frame}");
        two.push_str(&json);
        let mut lines = two.as_bytes();
        for _ in 0..2 {
            assert!(matches!(read_frame(&mut lines, max), Frame::Line(_)));
        }
        assert!(matches!(read_frame(&mut lines, max), Frame::Unterminated));

        assert_eq!(
            classify_frame(frame.as_bytes(), Some(cut.len()), false),
            Output::NotHanded
        );
    }

    /// A stand-in deck in `dir`: its `n`th connection is answered, once its
    /// first line is in, with `answers[n]` (`None`: it hangs up instead),
    /// and every line each connection sends is recorded until its peer
    /// closes it. Connections made in the half second after the scripted
    /// ones are recorded too, unanswered.
    #[cfg(unix)]
    pub(crate) fn stand_in_deck(
        dir: &Path,
        answers: Vec<Option<Vec<String>>>,
    ) -> (
        std::path::PathBuf,
        std::thread::JoinHandle<Vec<Vec<String>>>,
    ) {
        use std::io::BufRead;
        let path = dir.join("stand-in.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let serve = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for answer in answers {
                let (socket, _) = listener.accept().unwrap();
                let mut writer = socket.try_clone().unwrap();
                let mut lines = io::BufReader::new(socket).lines();
                let mut got = vec![lines.next().unwrap().unwrap()];
                match answer {
                    None => {
                        let _ = writer.shutdown(std::net::Shutdown::Both);
                    }
                    Some(answer) => {
                        for line in answer {
                            writeln!(writer, "{line}").unwrap();
                        }
                        got.extend(lines.map_while(Result::ok));
                    }
                }
                seen.push(got);
            }
            // A deck that answered its scripted connections still accepts
            // more, as a real one does: record whatever comes next.
            listener.set_nonblocking(true).unwrap();
            let until = Instant::now() + Duration::from_millis(500);
            while Instant::now() < until {
                match listener.accept() {
                    Ok((socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        seen.push(
                            io::BufReader::new(socket)
                                .lines()
                                .map_while(Result::ok)
                                .collect(),
                        );
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            seen
        });
        (path, serve)
    }

    /// Scenario: Point a one-shot's receipts question at a stand-in deck
    /// seven times, one connection each, answered in turn: that it serves
    /// receipts; nothing, as an older daemon answers a verb it cannot decode;
    /// a legacy batch; a capability of 0; a keepalive and then the
    /// capability; a refusal; and a hang-up. Only the two answers that name
    /// the capability on their own connection open it, the second
    /// connection's silence is not excused by the first's answer and is
    /// `TimedOut`, the answers without the capability are `Unsupported`, the
    /// hang-up is `Lost`, and the question is all any connection is sent.
    #[cfg(unix)]
    #[spec("inbox/claude/029")]
    #[test]
    fn claude_029_the_receipts_gate_decides_from_its_own_connections_answer() {
        let dir = crate::test_temp::tempdir().unwrap();
        let mut legacy = handout(ID1, 1, 1, None);
        legacy.attempt = None;
        let legacy = serde_json::to_string(&InboxWaitReply::messages(vec![legacy])).unwrap();
        let answers = vec![
            Some(vec![r#"{"one_shot_receipts":1}"#.to_string()]),
            Some(Vec::new()),
            Some(vec![legacy]),
            Some(vec![r#"{"one_shot_receipts":0}"#.to_string()]),
            Some(vec![
                r#"{"keepalive":true}"#.to_string(),
                r#"{"one_shot_receipts":1}"#.to_string(),
            ]),
            Some(vec![
                r#"{"refused":"unknown_token","error":"no such token"}"#.to_string(),
            ]),
            None,
        ];
        let rounds = answers.len();
        let (path, deck) = stand_in_deck(dir.path(), answers);
        let question = receipts_question(PANE, Some(AGENT), "token-1").unwrap();
        let wait = Duration::from_millis(300);
        let mut outcomes = Vec::new();
        for _ in 0..rounds {
            let started = Instant::now();
            let opened = Connection::open_receipts_within(&path, &question, wait);
            assert!(
                started.elapsed() < wait + Duration::from_secs(2),
                "the answer is waited for no longer than the wait"
            );
            outcomes.push(match opened {
                Opened::Ready(_) => "ready",
                Opened::Refused(_) => "refused",
                Opened::Unsupported => "unsupported",
                Opened::Lost => "lost",
                Opened::TimedOut => "timed out",
                Opened::Foreign => "foreign",
                Opened::Unreachable => "unreachable",
            });
        }
        assert_eq!(
            outcomes,
            [
                "ready",
                "timed out",
                "unsupported",
                "unsupported",
                "ready",
                "refused",
                "lost"
            ]
        );
        let seen = deck.join().unwrap();
        assert_eq!(seen.len(), rounds);
        for lines in seen {
            assert_eq!(
                lines,
                std::slice::from_ref(&question),
                "only the question is sent"
            );
        }
        let question: serde_json::Value = serde_json::from_str(&question).unwrap();
        assert_eq!(question["message_type"], "inbox_receipts");
        assert!(matches!(
            Connection::open_receipts_within(&dir.path().join("absent.sock"), "{}", wait),
            Opened::Unreachable
        ));
    }

    /// The slack a bounded wait in these pins may run past its budget, for a
    /// loaded box that schedules a thread late.
    #[cfg(unix)]
    pub(crate) const TOLERANCE: Duration = Duration::from_secs(2);

    /// Run `f` on a thread of its own and say how long it took, or `None` if
    /// it has not come back ten seconds past `budget`. What these pins catch
    /// is a call that blocks for good, which run inline would hang the test
    /// instead of failing it.
    #[cfg(unix)]
    pub(crate) fn timed<T: Send + 'static>(
        budget: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<(Duration, T)> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let got = f();
            let _ = tx.send((started.elapsed(), got));
        });
        rx.recv_timeout(budget + Duration::from_secs(10)).ok()
    }

    /// Scenario: Point a one-shot's receipts question at a socket whose accept
    /// queue is full and that never accepts, as a saturated deck's is. The gate
    /// comes back within its wait plus two seconds without opening it:
    /// `TimedOut` where a full queue parks the connect (Linux),
    /// `Unreachable` where the platform refuses it outright.
    #[cfg(unix)]
    #[spec("inbox/claude/033")]
    #[test]
    fn claude_033_the_receipts_gate_gives_up_on_a_deck_that_never_accepts() {
        use crate::platform::ipc::{QueueFull, SaturatedListener};
        let listener = SaturatedListener::new();
        let path = listener.path.clone();
        let wait = Duration::from_millis(300);
        let (took, opened) = timed(wait, move || {
            Connection::open_receipts_within(&path, "{}", wait)
        })
        .expect("the gate is still parked in connect(2)");
        assert!(took < wait + TOLERANCE, "the gate took {took:?}");
        match listener.behaviour {
            QueueFull::Parks => assert!(matches!(opened, Opened::TimedOut), "{opened:?}"),
            QueueFull::Refuses => assert!(matches!(opened, Opened::Unreachable), "{opened:?}"),
        }
    }

    /// Scenario: A stand-in deck accepts a one-shot's connection and reads
    /// nothing, and the question is larger than the socket can buffer, as any
    /// question is once the deck's side has filled up. The gate gives up as
    /// `TimedOut` within its wait plus two seconds instead of blocking in
    /// the write. A batch of fifty thousand attempts run on a connection the
    /// deck stops reading gives up its `may` questions when the approval runs
    /// out and its report after `RECEIPTS_ANSWER_WAIT`, within both plus two
    /// seconds.
    #[cfg(unix)]
    #[spec("inbox/claude/034")]
    #[test]
    fn claude_034_the_receipts_gate_gives_up_on_a_deck_that_does_not_read() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("deaf.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // The accepted socket stays open, unread, until the deck is joined.
        let deck = std::thread::spawn(move || listener.accept().map(|(socket, _)| socket));
        let question = format!("{{\"pad\":\"{}\"}}", "x".repeat(16 << 20));
        let wait = Duration::from_millis(300);
        let (took, opened) = timed(wait, move || {
            Connection::open_receipts_within(&path, &question, wait)
        })
        .expect("the gate is still blocked writing its question");
        assert!(took < wait + TOLERANCE, "the gate took {took:?}");
        assert!(matches!(opened, Opened::TimedOut), "{opened:?}");
        drop(deck.join());

        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let batch = OneShotBatch::new((1..=50_000).map(|a| handout(ID1, a, 1, None)).collect());
        let (took, ran) = timed(wait + RECEIPTS_ANSWER_WAIT, move || {
            let mut connection = Connection::over(ours, MAX_LINE_BYTES).unwrap();
            batch.run(
                &mut connection,
                &Pinned::Unpinned,
                wait,
                &mut Vec::new(),
                &[],
                Receiver::Hook,
                None,
            )
        })
        .expect("the batch is still blocked writing");
        assert!(
            took < wait + RECEIPTS_ANSWER_WAIT + TOLERANCE,
            "the batch took {took:?}"
        );
        assert!(
            ran.outputs
                .iter()
                .all(|output| *output == Output::NotHanded)
        );
        drop(theirs);
    }

    /// Scenario: A keepalive and an answer that the deck serves receipts are
    /// already waiting on a one-shot's connection when its deadline passes:
    /// reading with that deadline returns neither. Then a stand-in deck sends
    /// a keepalive every two milliseconds until past the wait and only then
    /// the answer; a prompt's commit against it gives up as timed out within
    /// the wait plus two seconds, and the question is the only line the deck
    /// reads.
    #[cfg(unix)]
    #[spec("inbox/claude/035")]
    #[test]
    fn claude_035_an_answer_after_the_deadline_is_not_taken_behind_keepalives() {
        use std::io::BufRead;
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut connection = Connection::over(ours, MAX_LINE_BYTES).unwrap();
        writeln!(theirs, r#"{{"keepalive":true}}"#).unwrap();
        writeln!(theirs, r#"{{"one_shot_receipts":1}}"#).unwrap();
        let deadline = Instant::now();
        // Long enough for the reader thread to have queued both lines.
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            connection.next(Some(deadline)).is_none(),
            "a line waiting past the deadline is not taken"
        );
        assert!(
            connection.next(None).is_some_and(|reply| reply.keepalive),
            "the lines were there"
        );

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("chatty.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let wait = Duration::from_millis(300);
        let deck = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut writer = socket.try_clone().unwrap();
            let mut lines = io::BufReader::new(socket).lines();
            let mut got = vec![lines.next().unwrap().unwrap()];
            let until = Instant::now() + wait + Duration::from_millis(500);
            while Instant::now() < until {
                if writeln!(writer, r#"{{"keepalive":true}}"#).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let _ = writeln!(writer, r#"{{"one_shot_receipts":1}}"#);
            got.extend(lines.map_while(Result::ok));
            got
        });
        let attempt = crate::event::InboxAttemptRef {
            id: ID1.to_string(),
            a: 1,
            e: 1,
        };
        let (took, committed) = timed(wait, move || {
            crate::inbox_prompt::commit_attempts(
                &path,
                crate::inbox_prompt::CommitFor {
                    pane_id: PANE,
                    agent_id: Some(AGENT),
                    token: "token-1",
                },
                vec![attempt],
                &Pinned::Unpinned,
                wait,
            )
        })
        .expect("the commit is still waiting");
        assert!(took < wait + TOLERANCE, "the commit took {took:?}");
        assert_eq!(committed, crate::inbox_prompt::Committed::TimedOut);
        let seen = deck.join().unwrap();
        assert_eq!(seen.len(), 1, "only the question: {seen:?}");
        let question: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(question["message_type"], "inbox_receipts");
    }

    /// Scenario: A stand-in deck floods a one-shot's connection with 64 KiB
    /// keepalive lines that the one-shot does not read. The deck soon cannot
    /// write more, having pushed no more than the connection's queue of lines
    /// and the socket's buffers rather than everything it had; and against a
    /// deck that floods keepalives and never answers, the gate is
    /// `TimedOut` within its wait plus two seconds.
    #[cfg(unix)]
    #[spec("inbox/claude/036")]
    #[test]
    fn claude_036_a_reply_flood_stays_bounded() {
        use std::io::BufRead;
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let _connection = Connection::over(ours, MAX_LINE_BYTES).unwrap();
        let line = format!(
            "{{\"keepalive\":true,\"pad\":\"{}\"}}\n",
            "x".repeat(64 * 1024)
        );
        // What the one-shot may hold, with room for the socket's own buffers;
        // a queue without a bound takes in all of `flood`.
        let bound = (MAX_QUEUED_LINES + 2) * line.len() + (16 << 20);
        let flood = 64 << 20;
        theirs.set_nonblocking(true).unwrap();
        let (mut pushed, mut at) = (0usize, 0usize);
        let mut stalled: Option<Instant> = None;
        while pushed <= flood {
            match (&theirs).write(&line.as_bytes()[at..]) {
                Ok(n) => {
                    pushed += n;
                    at = (at + n) % line.len();
                    stalled = None;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if stalled.get_or_insert_with(Instant::now).elapsed()
                        > Duration::from_millis(500)
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("the flood failed: {error}"),
            }
        }
        assert!(
            pushed <= bound,
            "the one-shot took in {pushed} bytes it never read (bound {bound})"
        );

        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("flood.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let deck = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut writer = socket.try_clone().unwrap();
            let mut lines = io::BufReader::new(socket).lines();
            let _question = lines.next();
            while writeln!(writer, r#"{{"keepalive":true}}"#).is_ok() {}
        });
        let wait = Duration::from_millis(300);
        let (took, opened) = timed(wait, move || {
            Connection::open_receipts_within(&path, "{}", wait)
        })
        .expect("the gate is still reading keepalives");
        assert!(took < wait + TOLERANCE, "the gate took {took:?}");
        assert!(matches!(opened, Opened::TimedOut), "{opened:?}");
        drop(opened);
        deck.join().unwrap();
    }

    /// Scenario: A line is on its way when a one-shot reads its connection
    /// with a deadline, and the clock, read once before the receive and once
    /// after it, has passed the deadline by the second reading, as when the
    /// thread is descheduled between its check and the receive. The read
    /// returns nothing, and the line is still there for the fence; with the
    /// clock not past the deadline the same read returns the next line; and
    /// a line held back this way is first for a later read without a deadline.
    #[cfg(unix)]
    #[spec("inbox/claude/039")]
    #[test]
    fn claude_039_a_line_received_after_the_deadline_is_not_returned_for_it() {
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut connection = Connection::over(ours, MAX_LINE_BYTES).unwrap();
        let base = Instant::now();
        // Far enough off that only the scripted clock can pass it.
        let deadline = base + Duration::from_secs(3600);
        let late = deadline + Duration::from_secs(1);
        // The clock read before the receive and after it.
        let script = |readings: [Instant; 2]| {
            let mut readings = readings.into_iter();
            move || readings.next().expect("the clock is read twice")
        };

        writeln!(theirs, r#"{{"one_shot_receipts":1}}"#).unwrap();
        assert!(
            connection
                .next_by(Some(deadline), &mut script([base, late]))
                .is_none(),
            "a line received after the deadline is not returned for it"
        );
        let arrived = connection.arrivals().arrived();
        assert_eq!(arrived.lines.len(), 1, "the fence still sees it");
        assert_eq!(arrived.lines[0].one_shot_receipts, Some(1));

        writeln!(theirs, r#"{{"one_shot_receipts":2}}"#).unwrap();
        let got = connection.next_by(Some(deadline), &mut script([base, base]));
        assert_eq!(
            got.and_then(|reply| reply.one_shot_receipts),
            Some(2),
            "in time, the line is returned"
        );

        writeln!(theirs, r#"{{"one_shot_receipts":3}}"#).unwrap();
        assert!(
            connection
                .next_by(Some(deadline), &mut script([base, late]))
                .is_none()
        );
        assert_eq!(
            connection
                .next(None)
                .and_then(|reply| reply.one_shot_receipts),
            Some(3),
            "the line held back is first for the next read"
        );
    }

    /// Scenario: Point a one-shot's receipts question at a socket of this
    /// user's own whose mode lets no one connect, so the connect is denied
    /// as it is for another user's private socket. The gate is `Foreign`,
    /// neither `Unreachable` nor `Unsupported`, and the listener accepts no
    /// connection, so the question, which carries the token, is never
    /// written. The diagnostic said for it is fixed text that claims neither
    /// cause, another user's deck or the socket's permissions, over the
    /// other, and names the remedy for each.
    #[cfg(unix)]
    #[spec("inbox/claude/041")]
    #[test]
    fn claude_041_a_socket_denied_to_this_user_is_refused_before_the_question() {
        use std::os::unix::fs::PermissionsExt;
        if crate::platform::paths::current_uid() == 0 {
            eprintln!("SKIP: root connects whatever a socket's mode says");
            return;
        }
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("foreign.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let question = receipts_question(PANE, Some(AGENT), "token-1").unwrap();
        let opened = Connection::open_receipts_within(&path, &question, Duration::from_millis(300));
        assert!(matches!(opened, Opened::Foreign), "{opened:?}");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "no connection was made, so no question was sent"
        );
        assert!(
            !RECEIPTS_FOREIGN_SOCKET.contains("belongs to another user"),
            "a same-user socket is not another user's: {RECEIPTS_FOREIGN_SOCKET}"
        );
        for says in [
            "refused this hook access",
            "runs as another user",
            "permissions of the socket or a directory above it",
            "DOT_AGENT_DECK_SOCKET",
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            "the default socket included",
            "asked it nothing",
        ] {
            assert!(
                RECEIPTS_FOREIGN_SOCKET.contains(says),
                "{says:?} missing: {RECEIPTS_FOREIGN_SOCKET}"
            );
        }
    }

    /// Scenario: Point a one-shot's receipts question at a stand-in deck that
    /// says nothing within the wait, then at one that answers without the
    /// capability, then at one that hangs up unanswered, and ask once more
    /// with the deadline already spent. The silent deck and the spent
    /// deadline are `TimedOut`, the spent one without connecting at all, the
    /// answer without the capability is `Unsupported` and the hang-up is
    /// `Lost`. The four notices differ: the timeout one says the deck did not
    /// answer in time, that no message was taken and that the messages stay
    /// queued; the commit's timeout promises nothing queued; the lost one
    /// says the connection was lost and how to check the deck is running;
    /// and only the unsupported one, which follows a real answer, calls the
    /// deck most likely older.
    #[cfg(unix)]
    #[spec("inbox/claude/044")]
    #[test]
    fn claude_044_a_receipts_timeout_is_told_apart_from_a_deck_without_receipts() {
        let dir = crate::test_temp::tempdir().unwrap();
        let (path, deck) = stand_in_deck(
            dir.path(),
            vec![
                Some(Vec::new()),
                Some(vec![r#"{"one_shot_receipts":0}"#.to_string()]),
                None,
            ],
        );
        let question = receipts_question(PANE, Some(AGENT), "token-1").unwrap();
        let wait = Duration::from_millis(300);
        let silent = Connection::open_receipts_within(&path, &question, wait);
        assert!(matches!(silent, Opened::TimedOut), "{silent:?}");
        drop(silent);
        let negative = Connection::open_receipts_within(&path, &question, wait);
        assert!(matches!(negative, Opened::Unsupported), "{negative:?}");
        drop(negative);
        let hung_up = Connection::open_receipts_within(&path, &question, wait);
        assert!(matches!(hung_up, Opened::Lost), "{hung_up:?}");
        drop(hung_up);
        assert_eq!(deck.join().unwrap().len(), 3);

        let spent = dir.path().join("spent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&spent).unwrap();
        listener.set_nonblocking(true).unwrap();
        let opened = Connection::open_receipts_by(&spent, &question, Instant::now());
        assert!(matches!(opened, Opened::TimedOut), "{opened:?}");
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "a spent deadline connects to nothing"
        );

        assert_ne!(RECEIPTS_TIMED_OUT, RECEIPTS_UNSUPPORTED);
        for says in [
            "did not answer in time",
            "took no message",
            "queued",
            "hooks install --agent claude-code",
        ] {
            assert!(
                RECEIPTS_TIMED_OUT.contains(says),
                "{says:?} missing: {RECEIPTS_TIMED_OUT}"
            );
        }
        assert!(RECEIPTS_UNSUPPORTED.contains("most likely older"));
        let notices = [
            RECEIPTS_UNSUPPORTED,
            RECEIPTS_TIMED_OUT,
            RECEIPTS_COMMIT_TIMED_OUT,
            RECEIPTS_CONNECTION_LOST,
            RECEIPTS_COMMIT_UNSUPPORTED,
            RECEIPTS_COMMIT_CONNECTION_LOST,
        ];
        for (i, a) in notices.iter().enumerate() {
            for b in &notices[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for notice in [
            RECEIPTS_TIMED_OUT,
            RECEIPTS_COMMIT_TIMED_OUT,
            RECEIPTS_CONNECTION_LOST,
            RECEIPTS_COMMIT_CONNECTION_LOST,
        ] {
            assert!(
                !notice.contains("most likely"),
                "no answer is no evidence the deck is older: {notice}"
            );
        }
        for says in [
            "did not answer in time",
            "did not confirm",
            "took no further message",
            "hooks install --agent claude-code",
        ] {
            assert!(
                RECEIPTS_COMMIT_TIMED_OUT.contains(says),
                "{says:?} missing: {RECEIPTS_COMMIT_TIMED_OUT}"
            );
        }
        assert!(
            !RECEIPTS_COMMIT_TIMED_OUT.contains("queued"),
            "the waiter already took the message a commit names: {RECEIPTS_COMMIT_TIMED_OUT}"
        );
        for says in [
            "connection to the deck daemon was lost before it answered",
            "took no message",
            "confirmed no receipt",
            "dot-agent-deck daemon status",
        ] {
            assert!(
                RECEIPTS_CONNECTION_LOST.contains(says),
                "{says:?} missing: {RECEIPTS_CONNECTION_LOST}"
            );
        }
        for not in ["older", "queued", "hooks install"] {
            assert!(
                !RECEIPTS_CONNECTION_LOST.contains(not),
                "{not:?}: a lost connection says nothing of versions or the queue: {RECEIPTS_CONNECTION_LOST}"
            );
        }
    }
}
