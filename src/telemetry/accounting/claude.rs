//! PRD #1858: Claude Code's usage and last turn, from its own transcript.
//!
//! Claude Code keeps its own running total of the session's usage — tokens
//! and its cost estimate per model, its subagents included — and appends it
//! to the session's transcript as a `cost-state` record when the session
//! ends. That total, not a sum of the transcript's per-request usage, is the
//! source: PRD #633's discovery found transcript sums short of Claude Code's
//! own total in 592 of 618 sessions. The transcript's `assistant` and `user`
//! records say how its last turn ended.
//!
//! A resumed session starts from the total its last run left, so a
//! `cost-state` already in the transcript when the run began is that run's,
//! not this one's. A total is credited to the run as a number only on
//! evidence that it is wholly the run's, or counted from a baseline the
//! transcript places before the run; in any doubt it is
//! [`Unavailable::Incomplete`]. Where the run's part of a transcript begins
//! is read from the transcript itself
//! ([`ClaudeTranscriptSummary::set_run_start`]): the first record stamped at
//! or after the run's start, after records stamped before it, is the
//! boundary, wherever the reader was when it got there, and the last
//! `cost-state` before it is the baseline. The evidence that a total is
//! wholly the run's is a total that itself started during the run
//! (`startTime`), or a transcript created after the run began (its creation
//! time, where the file system records one). A total carried from before the
//! run with neither a baseline nor that evidence — after records with no
//! stamp, in a transcript older than the run, or with no creation time to
//! tell — is incomplete, as is one that went down in any count since its
//! baseline.

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::{
    AgentTurnEnd, AgentUsage, Field, Ident, JsonlReader, LineSummary, MicroUsd, OptScalar, OptStr,
    ReaderStats, Unavailable, agent_cost, input_plus_cache, usage_unavailable,
};

/// An incremental reader of one Claude Code transcript.
pub type ClaudeTranscriptReader = JsonlReader<ClaudeTranscriptSummary>;

/// One model's entry in a `cost-state` record's `modelUsage`, in Claude
/// Code's own convention: input without the cache.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct ModelTotal {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost_usd: Option<f64>,
}

/// One `cost-state` record: Claude Code's own total for the session so far.
#[derive(Debug, Clone, Default, PartialEq)]
struct CostState {
    models: BTreeMap<Option<Ident>, ModelTotal>,
    has_unknown_model_cost: bool,
    /// When the total Claude Code keeps started (`startTime`). Two sessions'
    /// totals with the same start are one running total, carried from one
    /// session into the next.
    start_time: Option<i64>,
}

/// What a Claude Code transcript says about a run, as
/// [`ClaudeTranscriptReader`] accumulates it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeTranscriptSummary {
    /// The last `cost-state` record read: `Err` when it was there but not in
    /// a shape the reader knows.
    latest: Option<Result<CostState, ()>>,
    /// The `cost-state` the transcript already held when the run began.
    baseline: Option<CostState>,
    sealed: bool,
    /// A `cost-state` was read after the baseline was sealed.
    total_since_seal: bool,
    last_turn: Option<AgentTurnEnd>,
    /// The last turn before the latest prompt-like `user` record, restored
    /// when that record turns out to have been a local command (`/exit`).
    before_user: Option<Option<AgentTurnEnd>>,
    /// When the run began, in milliseconds since the Unix epoch
    /// ([`Self::set_run_start`]); `None` reads every record as the run's.
    run_start_ms: Option<i64>,
    /// Which part of the transcript the reader is in, by its records'
    /// stamps, relative to the run's start.
    part: Part,
    /// The file was created before the run began, where its creation time
    /// is known ([`Self::note_birth`]).
    born_before_run: Option<bool>,
    /// A record has been read: one that comes before the first stamped one
    /// is history with no stamp to place it by.
    records: bool,
}

/// Where in a transcript the reader is, relative to the run's start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Part {
    /// No stamped record read yet.
    #[default]
    Unplaced,
    /// Records stamped before the run began: an earlier run's.
    Before,
    /// The run's own records.
    Run,
}

impl ClaudeTranscriptSummary {
    /// Mark everything read so far as the transcript's state when the run
    /// began: its last `cost-state` becomes the baseline the run's usage is
    /// counted from, and its last turn is forgotten. The reader does this
    /// when a record stamped at or after the run's start follows records
    /// stamped before it ([`Self::set_run_start`]). A second call does
    /// nothing.
    pub fn seal_baseline(&mut self) {
        let baseline = match &self.latest {
            Some(Ok(state)) => Some(state.clone()),
            _ => None,
        };
        self.seal(baseline);
    }

    fn seal(&mut self, baseline: Option<CostState>) {
        if self.sealed {
            return;
        }
        self.sealed = true;
        self.baseline = baseline;
        self.last_turn = None;
        self.before_user = None;
    }

    /// The run this transcript is read for began at `started`, in
    /// nanoseconds since the Unix epoch: records stamped before it are an
    /// earlier run's, and the first one stamped at or after it is the
    /// boundary (see the module docs). Kept when the reader starts over.
    pub fn set_run_start(&mut self, started: u64) {
        self.run_start_ms = i64::try_from(started / 1_000_000).ok();
    }

    /// When the file was created, where known: a transcript created after
    /// the run began holds only the run's records, and one created before it
    /// holds an earlier run's.
    pub fn note_birth(&mut self, born: Option<SystemTime>) {
        let (Some(start), Some(born)) = (self.run_start_ms, born) else {
            return;
        };
        if let Ok(since) = born.duration_since(SystemTime::UNIX_EPOCH) {
            self.born_before_run =
                Some(i64::try_from(since.as_millis()).is_ok_and(|ms| ms < start));
        }
    }

    /// A record stamped `stamp` was read, before the record itself is
    /// applied, so the record that crosses the boundary is the run's.
    fn observe_stamp(&mut self, stamp: &str) {
        let Some(start) = self.run_start_ms else {
            return;
        };
        let Some(at) = chrono::DateTime::parse_from_rfc3339(stamp)
            .ok()
            .map(|t| t.timestamp_millis())
        else {
            return;
        };
        match self.part {
            Part::Run => {}
            _ if at < start => self.part = Part::Before,
            Part::Before => {
                self.seal_baseline();
                self.part = Part::Run;
            }
            Part::Unplaced => {
                // Records with no stamp came first: they may be an earlier
                // run's, and nothing places a total among them before or
                // after the run's start, so none of them is a baseline.
                if self.records {
                    self.seal(None);
                }
                self.part = Part::Run;
            }
        }
    }

    pub fn last_turn(&self) -> Option<&AgentTurnEnd> {
        self.last_turn.as_ref()
    }

    /// When the running total of the last `cost-state` read started.
    fn tracker_start(&self) -> Option<i64> {
        match &self.latest {
            Some(Ok(state)) => state.start_time,
            _ => None,
        }
    }

    /// The last turn read, unless nothing places the records it came from
    /// in the run.
    fn placed_turn(&self) -> Option<AgentTurnEnd> {
        let placed = match self.part {
            _ if self.run_start_ms.is_none() => true,
            Part::Before => false,
            _ if self.sealed => true,
            // No record before the first one the run could have written: the
            // run's own, unless the file is older than the run.
            _ => self.born_before_run != Some(true),
        };
        if placed { self.last_turn.clone() } else { None }
    }

    /// The run's usage. Without a run start, every total read counts as the
    /// run's own.
    pub fn usage(&self) -> AgentUsage {
        self.usage_from(self)
    }

    /// The run's usage from the running total this transcript's last
    /// `cost-state` carries, counted from what `first` — the earliest
    /// transcript the run read that carries the same total, or this one —
    /// places before the run.
    fn usage_from(&self, first: &Self) -> AgentUsage {
        let models = match &self.latest {
            None => usage_unavailable(Unavailable::NotRecorded),
            // No total since the boundary, or only an earlier run's records:
            // this run wrote none.
            Some(_) if self.sealed && !self.total_since_seal => {
                usage_unavailable(Unavailable::NotRecorded)
            }
            Some(_) if self.run_start_ms.is_some() && self.part == Part::Before => {
                usage_unavailable(Unavailable::NotRecorded)
            }
            Some(latest) => {
                let own_total = latest.as_ref().is_ok_and(|latest| {
                    self.run_start_ms
                        .zip(latest.start_time)
                        .is_some_and(|(start, total)| total >= start)
                });
                match (first.base(own_total), latest) {
                    (Err(reason), _) => usage_unavailable(reason),
                    (Ok(_), Err(())) => usage_unavailable(Unavailable::UnrecognizedFormat),
                    (Ok(base), Ok(latest)) => {
                        models_since(base.unwrap_or(&CostState::default()), latest)
                    }
                }
            }
        };
        AgentUsage {
            models,
            quota: Field::Unavailable(Unavailable::NoChannel),
            last_turn: self.placed_turn(),
        }
    }

    /// What a total of this transcript's running total is counted from:
    /// `None` for a total that is wholly the run's, else the baseline before
    /// the boundary, or why neither is known. `own_total` says the total
    /// itself started during the run.
    fn base(&self, own_total: bool) -> Result<Option<&CostState>, Unavailable> {
        if self.run_start_ms.is_some()
            && !self.sealed
            && self.part != Part::Before
            && self.born_before_run == Some(true)
        {
            // A file older than the run with no record stamped before it:
            // its stamps, and so a total's start, are not on the run's clock.
            return Err(Unavailable::Incomplete);
        }
        if own_total {
            return Ok(None);
        }
        if let Some(baseline) = self.baseline.as_ref().filter(|_| self.sealed) {
            return Ok(Some(baseline));
        }
        if self.run_start_ms.is_none() {
            return Ok(None);
        }
        if self.part == Part::Before || self.sealed {
            // Nothing read at or after the run's start to place a baseline
            // by, or no total before the boundary to count from.
            return Err(Unavailable::Incomplete);
        }
        // Nothing was read before the run's first record: the whole total is
        // the run's only when the file is known to be newer than the run.
        match self.born_before_run {
            Some(false) => Ok(None),
            Some(true) | None => Err(Unavailable::Incomplete),
        }
    }

    fn observe_record(&mut self, record: ClaudeRecord) {
        if let Some(stamp) = record.timestamp.0.as_deref() {
            self.observe_stamp(stamp);
        }
        self.records = true;
        let flag = |v: &OptScalar| v.as_bool() == Some(true);
        if flag(&record.sidechain) {
            return;
        }
        match record.kind.0.as_deref() {
            Some("cost-state") => {
                self.latest = Some(record.model_usage.ok_or(()).and_then(|usage| {
                    usage
                        .0
                        .map(|models| CostState {
                            models,
                            has_unknown_model_cost: flag(&record.has_unknown_model_cost),
                            start_time: record
                                .start_time
                                .count()
                                .and_then(|t| i64::try_from(t).ok()),
                        })
                        .ok_or(())
                }));
                if self.sealed {
                    self.total_since_seal = true;
                }
            }
            Some("assistant") => {
                self.before_user = None;
                if flag(&record.api_error) {
                    self.last_turn = Some(AgentTurnEnd::Failed {
                        // A kind that is not a lowercase identifier is one
                        // the deck does not know: `other`, never its text.
                        kind: record.error.as_str().map(|kind| {
                            Ident::new(kind)
                                .filter(|k| k.as_str().bytes().all(|b| !b.is_ascii_uppercase()))
                                .or_else(|| Ident::new("other"))
                                .expect("`other` is an identifier")
                        }),
                        status: record.api_error_status.http_status(),
                    });
                    return;
                }
                let stop = record.message.and_then(|m| m.stop_reason);
                self.last_turn = Some(match stop.as_deref() {
                    Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal") => {
                        AgentTurnEnd::Completed
                    }
                    Some("tool_use" | "pause_turn") | None => AgentTurnEnd::MidTurn,
                    Some(_) => AgentTurnEnd::Unknown,
                });
            }
            Some("user") => {
                if record.interrupted.as_str().is_some() {
                    self.before_user = None;
                    self.last_turn = Some(AgentTurnEnd::Interrupted);
                } else if !flag(&record.meta) && !flag(&record.compact_summary) {
                    self.before_user = Some(self.last_turn.take());
                    self.last_turn = Some(AgentTurnEnd::MidTurn);
                }
            }
            Some("system") if record.subtype.as_str() == Some("local_command") => {
                // The `user` record before it was a local command such as
                // `/exit`, not a prompt: no turn started.
                if let Some(before) = self.before_user.take() {
                    self.last_turn = before;
                }
            }
            _ => {}
        }
    }
}

/// Each model's growth from `base` to `latest`, the cost estimate with it.
/// A model `base` had that `latest` lost, or a count that went down, is
/// incomplete; a model the run did not use again is not credited.
fn models_since(base: &CostState, latest: &CostState) -> Vec<super::ModelUsage> {
    if base
        .models
        .keys()
        .any(|model| !latest.models.contains_key(model))
    {
        return usage_unavailable(Unavailable::Incomplete);
    }
    let mut acc = super::UsageAcc::default();
    for (model, now) in &latest.models {
        let before = base.models.get(model).copied().unwrap_or_default();
        let grown = (|| {
            Some(ModelTotal {
                input: now.input.checked_sub(before.input)?,
                output: now.output.checked_sub(before.output)?,
                cache_read: now.cache_read.checked_sub(before.cache_read)?,
                cache_write: now.cache_write.checked_sub(before.cache_write)?,
                cost_usd: match (now.cost_usd, before.cost_usd) {
                    (Some(a), Some(b)) if a >= b => Some(a - b),
                    (Some(a), None) if before == ModelTotal::default() => Some(a),
                    _ => None,
                },
            })
        })();
        let Some(grown) = grown else {
            let none = Unavailable::Incomplete;
            acc.add(
                model.clone(),
                Field::Unavailable(none),
                Field::Unavailable(none),
            );
            continue;
        };
        let counts = |t: &ModelTotal| (t.input, t.output, t.cache_read, t.cache_write);
        if counts(&grown) == (0, 0, 0, 0) && counts(&before) != (0, 0, 0, 0) {
            // A model the run did not use again: nothing to credit.
            continue;
        }
        let tokens = input_plus_cache(
            grown.input,
            grown.output,
            grown.cache_read,
            grown.cache_write,
        );
        let cost = if latest.has_unknown_model_cost {
            Field::Unavailable(Unavailable::UnknownPrice)
        } else {
            agent_cost(grown.cost_usd.and_then(MicroUsd::from_usd), &tokens)
        };
        acc.add(model.clone(), Field::ProviderReported(tokens), cost);
    }
    acc.finish()
}

/// One Claude Code session a run's events named, as far as its transcript
/// was read.
pub struct Observed<'a> {
    /// What was read of its transcript, if it was found.
    pub summary: Option<&'a ClaudeTranscriptSummary>,
    /// The transcript was readable at the run's end.
    pub read: bool,
    /// The transcript was read from, then could not be.
    pub cut_short: bool,
}

/// One run's usage from the transcripts of every Claude Code session it
/// held, in the order the run saw them. Claude Code can carry one running
/// total from a session into the next (a `/clear`, or a resume), so the
/// transcripts whose last totals share a start (`startTime`) hold one total:
/// it is counted once, as the last of them the run read carries it, from
/// what the earliest of them places before the run. The rest are combined as
/// separate sessions ([`super::combine_sessions`]). A session whose
/// transcript was not read is an unknown part of the run — its total
/// incomplete, its last turn unknown when it was the last — unless what was
/// read of it carries a total that a later transcript the run read carries
/// too, which then holds it.
pub fn combine_transcripts(sessions: &[Observed]) -> AgentUsage {
    let start = |s: &Observed| s.summary.and_then(ClaudeTranscriptSummary::tracker_start);
    let parts = sessions
        .iter()
        .enumerate()
        .filter_map(|(i, session)| {
            let shared = start(session);
            let carried_on = shared.is_some_and(|total| {
                sessions[i + 1..]
                    .iter()
                    .any(|later| later.read && start(later) == Some(total))
            });
            if carried_on {
                return None;
            }
            let Some(summary) = session.summary.filter(|_| session.read) else {
                return Some(AgentUsage {
                    models: usage_unavailable(if session.cut_short {
                        Unavailable::Incomplete
                    } else {
                        Unavailable::NotRecorded
                    }),
                    quota: Field::Unavailable(Unavailable::NoChannel),
                    last_turn: Some(AgentTurnEnd::Unknown),
                });
            };
            let first = shared.and_then(|total| {
                sessions[..i]
                    .iter()
                    .find(|earlier| start(earlier) == Some(total))
            });
            Some(match first {
                None => summary.usage(),
                Some(Observed {
                    summary: Some(first),
                    read: true,
                    ..
                }) => summary.usage_from(first),
                // The total's earliest transcript was not read through: what
                // it placed before the run is not known.
                Some(_) => AgentUsage {
                    models: usage_unavailable(Unavailable::Incomplete),
                    ..summary.usage()
                },
            })
        })
        .collect();
    super::combine_sessions(parts)
}

impl LineSummary for ClaudeTranscriptSummary {
    const MARKERS: &'static [&'static [u8]] = &[
        b"\"type\":\"cost-state\"",
        b"\"type\":\"assistant\"",
        b"\"type\":\"user\"",
        b"\"subtype\":\"local_command\"",
    ];
    type Record = ClaudeRecord;

    fn observe(&mut self, record: ClaudeRecord, _stats: &mut ReaderStats) {
        self.observe_record(record);
    }

    fn reset(&mut self) {
        *self = ClaudeTranscriptSummary {
            run_start_ms: self.run_start_ms,
            ..ClaudeTranscriptSummary::default()
        };
    }

    fn malformed(&mut self, line: &[u8]) {
        self.records = true;
        // A total that did not parse is newer than the last one read, so
        // keeping the older one would report a figure the session has since
        // moved past: it is unrecognized instead.
        if super::line_is_candidate(line, &[Self::MARKERS[0]]) {
            self.latest = Some(Err(()));
            if self.sealed {
                self.total_since_seal = true;
            }
        } else {
            // A turn's record the reader could not read: how the last turn
            // ended is not known.
            self.before_user = None;
            self.last_turn = Some(AgentTurnEnd::Unknown);
        }
    }
}

// The narrow shape a transcript line is deserialised into. Every field not
// named here — the prompt, the reply, tool input and output, the working
// directory, the branch — is skipped by the parser without being kept, and
// every named field is lenient about its type, so an unexpected value costs
// that field rather than the line.

#[derive(Deserialize)]
pub struct ClaudeRecord {
    #[serde(rename = "type", default)]
    kind: OptStr,
    #[serde(default)]
    subtype: OptScalar,
    #[serde(rename = "isSidechain", default)]
    sidechain: OptScalar,
    #[serde(rename = "isMeta", default)]
    meta: OptScalar,
    #[serde(rename = "isCompactSummary", default)]
    compact_summary: OptScalar,
    #[serde(rename = "isApiErrorMessage", default)]
    api_error: OptScalar,
    /// The kind of an API error (`rate_limit`); its message is in the
    /// record's `message.content`, which is not read.
    #[serde(default)]
    error: OptScalar,
    #[serde(rename = "apiErrorStatus", default)]
    api_error_status: OptScalar,
    #[serde(rename = "interruptedMessageId", default)]
    interrupted: OptScalar,
    #[serde(default)]
    message: Option<StopReason>,
    #[serde(rename = "modelUsage", default)]
    model_usage: Option<ModelUsageMap>,
    #[serde(rename = "hasUnknownModelCost", default)]
    has_unknown_model_cost: OptScalar,
    #[serde(rename = "startTime", default)]
    start_time: OptScalar,
    /// When the record was written: RFC 3339.
    #[serde(default)]
    timestamp: OptStr,
}

/// A record's `message`, of which only `stop_reason` is kept; any other
/// shape is skipped.
pub struct StopReason {
    stop_reason: Option<String>,
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StopReason;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a message")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<StopReason, A::Error> {
                let mut stop_reason = None;
                while let Some(key) = map.next_key::<OptStr>()? {
                    if key.0.as_deref() == Some("stop_reason") {
                        stop_reason = map.next_value::<OptStr>()?.0;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(StopReason { stop_reason })
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<StopReason, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(StopReason { stop_reason: None })
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }

            fn visit_unit<E: de::Error>(self) -> Result<StopReason, E> {
                Ok(StopReason { stop_reason: None })
            }
        }
        d.deserialize_any(V)
    }
}

/// `modelUsage`: per model, its four token counts and cost. `None` when it
/// is not an object of objects each with all four counts, so a changed
/// format is unreadable rather than read as zero. Past
/// [`super::MAX_MODELS`] named models, usage is credited to no model.
pub struct ModelUsageMap(Option<BTreeMap<Option<Ident>, ModelTotal>>);

#[derive(Deserialize)]
struct ModelEntry {
    #[serde(rename = "inputTokens", default)]
    input: OptScalar,
    #[serde(rename = "outputTokens", default)]
    output: OptScalar,
    #[serde(rename = "cacheReadInputTokens", default)]
    cache_read: OptScalar,
    #[serde(rename = "cacheCreationInputTokens", default)]
    cache_write: OptScalar,
    #[serde(rename = "costUSD", default)]
    cost: OptScalar,
}

/// A `modelUsage` value that may not be an object.
enum MaybeEntry {
    Entry(Box<ModelEntry>),
    Other,
}

impl<'de> Deserialize<'de> for MaybeEntry {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = MaybeEntry;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a model's usage")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<MaybeEntry, A::Error> {
                ModelEntry::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(|e| MaybeEntry::Entry(Box::new(e)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<MaybeEntry, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MaybeEntry::Other)
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }

            fn visit_unit<E: de::Error>(self) -> Result<MaybeEntry, E> {
                Ok(MaybeEntry::Other)
            }
        }
        d.deserialize_any(V)
    }
}

impl<'de> Deserialize<'de> for ModelUsageMap {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ModelUsageMap;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("modelUsage")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ModelUsageMap, A::Error> {
                let mut models: BTreeMap<Option<Ident>, ModelTotal> = BTreeMap::new();
                let mut readable = true;
                while let Some(key) = map.next_key::<OptStr>()? {
                    let entry = map.next_value::<MaybeEntry>()?;
                    let MaybeEntry::Entry(entry) = entry else {
                        readable = false;
                        continue;
                    };
                    let (Some(input), Some(output)) = (entry.input.count(), entry.output.count())
                    else {
                        readable = false;
                        continue;
                    };
                    // Claude Code writes both cache counts on every entry,
                    // 0 included, so one that is missing, `null` or not a
                    // count is a changed format, as a missing input is.
                    let (Some(cache_read), Some(cache_write)) =
                        (entry.cache_read.count(), entry.cache_write.count())
                    else {
                        readable = false;
                        continue;
                    };
                    let model = key.0.as_deref().and_then(Ident::new);
                    let model = super::capped(&models, model);
                    let cost = entry.cost.amount();
                    match models.entry(model) {
                        std::collections::btree_map::Entry::Vacant(slot) => {
                            slot.insert(ModelTotal {
                                input,
                                output,
                                cache_read,
                                cache_write,
                                cost_usd: cost,
                            });
                        }
                        // Several models credited to no model, past the cap.
                        std::collections::btree_map::Entry::Occupied(mut slot) => {
                            let total = slot.get_mut();
                            total.input = total.input.saturating_add(input);
                            total.output = total.output.saturating_add(output);
                            total.cache_read = total.cache_read.saturating_add(cache_read);
                            total.cache_write = total.cache_write.saturating_add(cache_write);
                            total.cost_usd = total.cost_usd.zip(cost).map(|(a, b)| a + b);
                        }
                    }
                }
                Ok(ModelUsageMap(readable.then_some(models)))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ModelUsageMap, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ModelUsageMap(None))
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }

            fn visit_unit<E: de::Error>(self) -> Result<ModelUsageMap, E> {
                Ok(ModelUsageMap(None))
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ModelUsage, Provenance, Tokens};
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    const SENTINEL: &str = "SENTINELc1a0d3";

    fn cost_state(models: &[(&str, u64, u64, u64, u64, f64)]) -> String {
        let entries: Vec<String> = models
            .iter()
            .map(|(m, i, o, cr, cw, c)| {
                format!(
                    r#""{m}":{{"inputTokens":{i},"outputTokens":{o},"thinkingTokens":0,"cacheReadInputTokens":{cr},"cacheCreationInputTokens":{cw},"webSearchRequests":0,"costUSD":{c}}}"#
                )
            })
            .collect();
        format!(
            r#"{{"type":"cost-state","sessionId":"s","totalCostUSD":1.0,"totalLinesAdded":3,"startTime":1,"modelUsage":{{{}}},"hasUnknownModelCost":false}}"#,
            entries.join(",")
        )
    }

    fn assistant(stop: &str) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":false,"cwd":"/home/{SENTINEL}","message":{{"model":"claude-opus-5-5","stop_reason":"{stop}","content":[{{"type":"text","text":"{SENTINEL} reply"}}],"usage":{{"input_tokens":9}}}}}}"#
        )
    }

    fn prompt() -> String {
        format!(
            r#"{{"type":"user","isSidechain":false,"message":{{"role":"user","content":"{SENTINEL} prompt"}}}}"#
        )
    }

    fn read(lines: &[String]) -> ClaudeTranscriptReader {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let mut reader = ClaudeTranscriptReader::new(&path);
        reader.poll().unwrap();
        reader
    }

    fn model<'a>(usage: &'a AgentUsage, name: &str) -> &'a ModelUsage {
        usage
            .models
            .iter()
            .find(|m| m.model.as_ref().map(Ident::as_str) == Some(name))
            .unwrap_or_else(|| panic!("no {name} in {:?}", usage.models))
    }

    /// The session's own total is the source: tokens provider-reported with
    /// the cache counted in the input, cost as Claude Code's estimate, one
    /// entry per model, whatever the transcript's per-request usage says.
    #[test]
    fn the_cost_state_total_is_the_runs_usage() {
        let reader = read(&[
            prompt(),
            assistant("end_turn"),
            cost_state(&[
                ("claude-opus-5-5", 10, 20, 300, 40, 1.25),
                ("claude-haiku-5-5", 1, 2, 0, 0, 0.01),
            ]),
        ]);
        let usage = reader.summary().usage();
        let opus = model(&usage, "claude-opus-5-5");
        assert_eq!(
            opus.tokens,
            Field::ProviderReported(Tokens {
                input: 350,
                output: 20,
                cache_read: 300,
                cache_write: 40,
            })
        );
        assert_eq!(opus.cost, Field::Estimated(MicroUsd(1_250_000)));
        assert_eq!(
            model(&usage, "claude-haiku-5-5").cost,
            Field::Estimated(MicroUsd(10_000))
        );
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Completed));
        assert_eq!(usage.quota, Field::Unavailable(Unavailable::NoChannel));
    }

    /// A transcript with no total yet — a session still running, or one
    /// killed before Claude Code wrote it — has its usage not recorded,
    /// never zero.
    #[test]
    fn no_total_is_not_recorded() {
        let reader = read(&[prompt(), assistant("tool_use")]);
        let usage = reader.summary().usage();
        assert_eq!(usage.models.len(), 1);
        assert_eq!(
            usage.models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::MidTurn));
    }

    /// A resumed session's transcript already holds the last run's total:
    /// the run is credited only with what the total grew by since, and a
    /// model it did not use again is not credited at all.
    #[test]
    fn a_resumed_session_counts_from_the_baseline() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            cost_state(&[
                ("claude-opus-5-5", 10, 20, 100, 0, 1.0),
                ("claude-haiku-5-5", 5, 5, 0, 0, 0.5),
            ])
        )
        .unwrap();
        writeln!(file, "{}", assistant("end_turn")).unwrap();
        let mut reader = ClaudeTranscriptReader::new(&path);
        reader.poll().unwrap();
        reader.summary_mut().seal_baseline();
        assert_eq!(
            reader.summary().usage().models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded),
            "the earlier run's total is not this run's"
        );
        assert_eq!(reader.summary().last_turn(), None);
        writeln!(file, "{}", prompt()).unwrap();
        writeln!(
            file,
            "{}",
            cost_state(&[
                ("claude-opus-5-5", 15, 30, 150, 10, 1.5),
                ("claude-haiku-5-5", 5, 5, 0, 0, 0.5),
            ])
        )
        .unwrap();
        reader.poll().unwrap();
        let usage = reader.summary().usage();
        assert_eq!(usage.models.len(), 1, "{:?}", usage.models);
        let opus = model(&usage, "claude-opus-5-5");
        assert_eq!(
            opus.tokens,
            Field::ProviderReported(Tokens {
                input: 65,
                output: 10,
                cache_read: 50,
                cache_write: 10,
            })
        );
        assert_eq!(opus.cost, Field::Estimated(MicroUsd(500_000)));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::MidTurn));
    }

    /// Two transcripts whose totals share a start hold one running total
    /// carried across sessions: only the later counts. Totals with different
    /// starts are separate sessions and add up.
    #[test]
    fn a_total_carried_into_the_next_session_counts_once() {
        let with_start = |line: String, start: u64| {
            line.replace(r#""startTime":1"#, &format!(r#""startTime":{start}"#))
        };
        let first = read(&[with_start(
            cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 1.0)]),
            7,
        )]);
        let second = read(&[with_start(
            cost_state(&[("claude-opus-5-5", 15, 30, 0, 0, 1.5)]),
            7,
        )]);
        let other = read(&[with_start(
            cost_state(&[("claude-opus-5-5", 1, 1, 0, 0, 0.5)]),
            9,
        )]);
        let read = |s| Observed {
            summary: Some(s),
            read: true,
            cut_short: false,
        };
        let carried = combine_transcripts(&[read(first.summary()), read(second.summary())]);
        assert_eq!(
            model(&carried, "claude-opus-5-5").tokens,
            Field::ProviderReported(Tokens {
                input: 15,
                output: 30,
                cache_read: 0,
                cache_write: 0,
            })
        );
        let separate = combine_transcripts(&[read(first.summary()), read(other.summary())]);
        assert_eq!(
            model(&separate, "claude-opus-5-5").cost,
            Field::Estimated(MicroUsd(1_500_000))
        );
    }

    /// A total lower than the baseline, or one that lost a model the
    /// baseline had, is not a delta the reader can trust: incomplete.
    #[test]
    fn a_total_below_the_baseline_is_incomplete() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 1.0)])
        )
        .unwrap();
        let mut reader = ClaudeTranscriptReader::new(&path);
        reader.poll().unwrap();
        reader.summary_mut().seal_baseline();
        writeln!(
            file,
            "{}",
            cost_state(&[("claude-opus-5-5", 3, 4, 0, 0, 0.1)])
        )
        .unwrap();
        reader.poll().unwrap();
        let usage = reader.summary().usage();
        assert_eq!(
            usage.models[0].tokens,
            Field::Unavailable(Unavailable::Incomplete)
        );
        writeln!(
            file,
            "{}",
            cost_state(&[("claude-haiku-5-5", 30, 40, 0, 0, 1.0)])
        )
        .unwrap();
        reader.poll().unwrap();
        assert_eq!(
            reader.summary().usage().models,
            usage_unavailable(Unavailable::Incomplete)
        );
    }

    /// A subscription's zero cost for a model that used tokens is
    /// unavailable, and a model Claude Code could not price is too.
    #[test]
    fn a_zero_or_unknown_price_is_unavailable() {
        let reader = read(&[cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 0.0)])]);
        assert_eq!(
            reader.summary().usage().models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        let unknown = cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 2.0)]).replace(
            r#""hasUnknownModelCost":false"#,
            r#""hasUnknownModelCost":true"#,
        );
        let reader = read(&[unknown]);
        assert_eq!(
            reader.summary().usage().models[0].cost,
            Field::Unavailable(Unavailable::UnknownPrice)
        );
    }

    /// A total in a shape the reader does not know — a renamed count, a
    /// count that is `null` or not a number, cache counts included,
    /// `modelUsage` that is not an object — is
    /// unrecognized, never zero; so is a total line that does not parse.
    #[test]
    fn a_changed_format_is_unrecognized_not_zero() {
        for changed in [
            cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 1.0)])
                .replace("outputTokens", "outTokens"),
            cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 1.0)])
                .replace(r#""inputTokens":10"#, r#""inputTokens":"10""#),
            r#"{"type":"cost-state","modelUsage":[1,2]}"#.to_owned(),
            r#"{"type":"cost-state","modelUsage":{"claude-opus-5-5":7}}"#.to_owned(),
            // A cache count renamed, `null`, or of another type.
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)])
                .replace("cacheReadInputTokens", "cacheReadTokens"),
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)])
                .replace("cacheCreationInputTokens", "cacheWriteTokens"),
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)]).replace(
                r#""cacheReadInputTokens":5"#,
                r#""cacheReadInputTokens":null"#,
            ),
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)]).replace(
                r#""cacheCreationInputTokens":0"#,
                r#""cacheCreationInputTokens":{"ephemeral":3}"#,
            ),
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)]).replace(
                r#""cacheReadInputTokens":5"#,
                r#""cacheReadInputTokens":[5]"#,
            ),
            cost_state(&[("claude-opus-5-5", 10, 20, 5, 0, 1.0)]).replace(
                r#""cacheReadInputTokens":5"#,
                r#""cacheReadInputTokens":"5""#,
            ),
        ] {
            let reader = read(std::slice::from_ref(&changed));
            assert_eq!(
                reader.summary().usage().models[0].tokens,
                Field::Unavailable(Unavailable::UnrecognizedFormat),
                "{changed}"
            );
        }
        let reader = read(&[
            cost_state(&[("claude-opus-5-5", 10, 20, 0, 0, 1.0)]),
            r#"{"type":"cost-state","modelUsage":{"#.to_owned(),
        ]);
        assert_eq!(
            reader.summary().usage().models[0].tokens,
            Field::Unavailable(Unavailable::UnrecognizedFormat)
        );
    }

    /// How the last turn ended, from the records' kinds and flags only: an
    /// API error with its kind and status, an interrupt, a completed reply,
    /// a reply that asked for a tool, a prompt with no reply yet; and a
    /// local command such as `/exit`, a meta record, a compaction summary or
    /// a subagent's record does not start a turn.
    #[test]
    fn the_last_turn_is_read_from_kinds_and_flags_only() {
        let api_error = format!(
            r#"{{"type":"assistant","isSidechain":false,"isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"message":{{"model":"<synthetic>","stop_reason":"stop_sequence","content":[{{"type":"text","text":"{SENTINEL} limit"}}]}}}}"#
        );
        let interrupt = format!(
            r#"{{"type":"user","isSidechain":false,"interruptedMessageId":"m1","message":{{"role":"user","content":"{SENTINEL}"}}}}"#
        );
        let meta = r#"{"type":"user","isMeta":true,"message":{"content":"x"}}"#.to_owned();
        let compact =
            r#"{"type":"user","isCompactSummary":true,"message":{"content":"x"}}"#.to_owned();
        let local = r#"{"type":"system","subtype":"local_command","content":"x"}"#.to_owned();
        let sidechain =
            r#"{"type":"assistant","isSidechain":true,"message":{"stop_reason":"tool_use"}}"#
                .to_owned();
        let cases: Vec<(Vec<String>, AgentTurnEnd)> = vec![
            (
                vec![prompt(), api_error.clone()],
                AgentTurnEnd::Failed {
                    kind: Some(Ident::new("rate_limit").unwrap()),
                    status: Some(429),
                },
            ),
            (
                vec![prompt(), assistant("tool_use"), interrupt],
                AgentTurnEnd::Interrupted,
            ),
            (
                vec![prompt(), assistant("end_turn")],
                AgentTurnEnd::Completed,
            ),
            (
                vec![prompt(), assistant("max_tokens")],
                AgentTurnEnd::Completed,
            ),
            (vec![prompt(), assistant("tool_use")], AgentTurnEnd::MidTurn),
            (vec![assistant("end_turn"), prompt()], AgentTurnEnd::MidTurn),
            (vec![assistant("brand_new_reason")], AgentTurnEnd::Unknown),
            (
                vec![assistant("end_turn"), prompt(), local],
                AgentTurnEnd::Completed,
            ),
            (
                vec![assistant("end_turn"), meta, compact],
                AgentTurnEnd::Completed,
            ),
            (
                vec![assistant("end_turn"), sidechain],
                AgentTurnEnd::Completed,
            ),
        ];
        for (lines, want) in cases {
            let reader = read(&lines);
            assert_eq!(reader.summary().last_turn(), Some(&want), "{lines:?}");
        }
    }

    /// An error kind that is not identifier-shaped, or not lowercase, is
    /// kept as a failure of kind `other`; a status outside HTTP's range is
    /// dropped.
    #[test]
    fn a_strange_error_kind_or_status_is_dropped_not_kept() {
        let line = format!(
            r#"{{"type":"assistant","isApiErrorMessage":true,"error":"{SENTINEL} Bad","apiErrorStatus":42,"message":{{}}}}"#
        );
        let reader = read(&[line]);
        assert_eq!(
            reader.summary().last_turn(),
            Some(&AgentTurnEnd::Failed {
                kind: Ident::new("other"),
                status: None
            })
        );
    }

    /// Content planted in a prompt, a reply, a tool result, an error
    /// message, the working directory and a model name that is not an
    /// identifier reaches neither the summary nor its usage.
    #[test]
    fn content_never_leaves_the_reader() {
        let tool_result = format!(
            r#"{{"type":"user","toolUseResult":{{"stdout":"{SENTINEL}"}},"message":{{"content":[{{"type":"tool_result","content":"{SENTINEL}"}}]}}}}"#
        );
        let error = format!(
            r#"{{"type":"assistant","isApiErrorMessage":true,"error":"server_error","message":{{"content":[{{"type":"text","text":"{SENTINEL} error"}}]}}}}"#
        );
        let hostile_model = cost_state(&[(&format!("{SENTINEL} model"), 1, 1, 0, 0, 0.1)]);
        let reader = read(&[
            prompt(),
            assistant("end_turn"),
            tool_result,
            error,
            hostile_model,
        ]);
        let summary = format!("{:?}", reader.summary());
        assert!(!summary.contains(SENTINEL), "{summary}");
        let usage = format!("{:?}", reader.summary().usage());
        assert!(!usage.contains(SENTINEL), "{usage}");
        assert!(!format!("{reader:?}").contains(SENTINEL));
        let usage = reader.summary().usage();
        assert_eq!(usage.models[0].model, None);
        assert_eq!(
            usage.models[0].tokens.provenance(),
            Provenance::ProviderReported
        );
    }

    /// When the runs below began, in milliseconds since the Unix epoch.
    const RUN_START_MS: i64 = 1_800_000_000_000;

    fn total(models: &[(&str, u64, u64, u64, u64, f64)], start_ms: i64) -> String {
        cost_state(models).replace(r#""startTime":1"#, &format!(r#""startTime":{start_ms}"#))
    }

    fn stamped(line: String, at_ms: i64) -> String {
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        line.replacen('{', &format!(r#"{{"timestamp":"{at}","#), 1)
    }

    /// `lines` read for a run that began at [`RUN_START_MS`], in a file
    /// created at `born` (unknown when `None`).
    fn read_for_run(lines: &[String], born: Option<i64>) -> ClaudeTranscriptReader {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let mut reader = ClaudeTranscriptReader::new(&path);
        reader
            .summary_mut()
            .set_run_start(u64::try_from(RUN_START_MS).unwrap() * 1_000_000);
        reader.poll().unwrap();
        let born = born.map(|ms| {
            SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(u64::try_from(ms).unwrap())
        });
        reader.summary_mut().note_birth(born);
        reader
    }

    const BEFORE: i64 = RUN_START_MS - 3_600_000;
    const OPUS_100: &[(&str, u64, u64, u64, u64, f64)] = &[("claude-opus-5-5", 100, 0, 0, 0, 1.0)];
    const OPUS_120: &[(&str, u64, u64, u64, u64, f64)] = &[("claude-opus-5-5", 120, 0, 0, 0, 1.2)];

    fn opus_input(usage: &AgentUsage) -> Field<Tokens> {
        model(usage, "claude-opus-5-5").tokens
    }

    fn input(n: u64) -> Field<Tokens> {
        Field::ProviderReported(Tokens {
            input: n,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        })
    }

    /// A total carried from before the run is the run's whole only on
    /// evidence the transcript is the run's own — created after the run
    /// began — and counts from a baseline the stamps place before the run
    /// otherwise; with neither, it is incomplete, never the earlier run's
    /// total: no stamps and no creation time, history with no stamp before a
    /// record stamped exactly at the run's start, and a file older than the
    /// run whose stamps say every record is newer (a clock that moved).
    #[test]
    fn a_carried_total_needs_a_baseline_or_a_newer_file() {
        let incomplete = Field::Unavailable(Unavailable::Incomplete);
        // A case's name, its lines, when its file was created, and its tokens.
        type Case = (&'static str, Vec<String>, Option<i64>, Field<Tokens>);
        let cases: Vec<Case> = vec![
            (
                "no stamps, no creation time",
                vec![total(OPUS_100, BEFORE), prompt(), total(OPUS_120, BEFORE)],
                None,
                incomplete,
            ),
            (
                "no stamps, older file",
                vec![total(OPUS_100, BEFORE), prompt(), total(OPUS_120, BEFORE)],
                Some(BEFORE),
                incomplete,
            ),
            (
                "no stamps, a file created during the run",
                vec![prompt(), total(OPUS_120, BEFORE)],
                Some(RUN_START_MS + 1),
                input(120),
            ),
            (
                "unstamped history, then a record stamped exactly at the start",
                vec![
                    total(OPUS_100, BEFORE),
                    stamped(prompt(), RUN_START_MS),
                    total(OPUS_120, BEFORE),
                ],
                None,
                incomplete,
            ),
            (
                "unstamped history, a stamp at the start, and a newer file",
                vec![
                    total(OPUS_100, BEFORE),
                    stamped(prompt(), RUN_START_MS),
                    total(OPUS_120, BEFORE),
                ],
                Some(RUN_START_MS + 1),
                incomplete,
            ),
            (
                "an older file whose stamps are all after the start",
                vec![
                    stamped(prompt(), RUN_START_MS + 5),
                    total(OPUS_100, BEFORE),
                    stamped(prompt(), RUN_START_MS + 10),
                    total(OPUS_120, BEFORE),
                ],
                Some(BEFORE),
                incomplete,
            ),
            (
                "an older file whose stamps and total start are after the start",
                vec![
                    stamped(prompt(), RUN_START_MS + 5),
                    total(OPUS_120, RUN_START_MS + 5),
                ],
                Some(BEFORE),
                incomplete,
            ),
            (
                "stamps place the boundary: counted from the baseline",
                vec![
                    stamped(prompt(), BEFORE),
                    total(OPUS_100, BEFORE),
                    stamped(prompt(), RUN_START_MS),
                    total(OPUS_120, BEFORE),
                ],
                Some(BEFORE),
                input(20),
            ),
        ];
        for (name, lines, born, want) in cases {
            let reader = read_for_run(&lines, born);
            let usage = reader.summary().usage();
            let got = usage.models[0].tokens;
            if want == incomplete {
                assert_eq!(
                    usage.models,
                    usage_unavailable(Unavailable::Incomplete),
                    "{name}"
                );
            } else {
                assert_eq!(got, want, "{name}");
            }
        }
    }

    /// Resume a session, then `/clear` into a new one that carries the same
    /// running total, then exit: the run is credited with what the total grew
    /// by since the resumed session's baseline, not the new session's whole
    /// total, and a model the run did not use again is not credited.
    #[test]
    fn a_total_carried_from_a_resumed_session_counts_from_its_baseline() {
        let haiku = ("claude-haiku-5-5", 5, 5, 0, 0, 0.5);
        let resumed = read_for_run(
            &[
                stamped(prompt(), BEFORE),
                total(&[("claude-opus-5-5", 100, 0, 0, 0, 1.0), haiku], BEFORE),
                stamped(prompt(), RUN_START_MS + 1),
                total(&[("claude-opus-5-5", 110, 0, 0, 0, 1.1), haiku], BEFORE),
            ],
            Some(BEFORE),
        );
        let cleared = read_for_run(
            &[
                stamped(prompt(), RUN_START_MS + 2),
                stamped(assistant("end_turn"), RUN_START_MS + 3),
                total(&[("claude-opus-5-5", 120, 0, 0, 0, 1.25), haiku], BEFORE),
            ],
            Some(RUN_START_MS + 2),
        );
        let read = |s| Observed {
            summary: Some(s),
            read: true,
            cut_short: false,
        };
        let usage = combine_transcripts(&[read(resumed.summary()), read(cleared.summary())]);
        assert_eq!(usage.models.len(), 1, "{:?}", usage.models);
        assert_eq!(opus_input(&usage), input(20));
        assert_eq!(
            model(&usage, "claude-opus-5-5").cost,
            Field::Estimated(MicroUsd(250_000))
        );
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Completed));
    }

    /// A session the run named whose transcript was not read is covered only
    /// by explicit evidence that a later transcript carries its total: an
    /// unrelated later session whose total started before the deck saw it,
    /// or an older resumed session, leaves the run incomplete; one whose
    /// partly read total a later transcript carries is covered.
    #[test]
    fn an_unread_session_is_covered_only_by_a_shared_total() {
        let unread = Observed {
            summary: None,
            read: false,
            cut_short: false,
        };
        let read = |s| Observed {
            summary: Some(s),
            read: true,
            cut_short: false,
        };
        let unrelated = read_for_run(
            &[
                stamped(prompt(), RUN_START_MS + 2),
                total(OPUS_100, RUN_START_MS + 1),
            ],
            Some(RUN_START_MS + 1),
        );
        let older_resumed = read_for_run(
            &[
                stamped(prompt(), BEFORE),
                total(OPUS_100, BEFORE - 1),
                stamped(prompt(), RUN_START_MS + 2),
                total(OPUS_120, BEFORE - 1),
            ],
            Some(BEFORE),
        );
        for (name, later) in [("unrelated", &unrelated), ("older resumed", &older_resumed)] {
            let usage = combine_transcripts(&[
                Observed {
                    summary: None,
                    read: false,
                    cut_short: false,
                },
                read(later.summary()),
            ]);
            // The unread session stands for an unknown part, which makes
            // the run's total unavailable.
            assert!(
                usage.models.iter().any(|m| m.tokens.value().is_none()),
                "{name}: {:?}",
                usage.models
            );
        }
        let usage = combine_transcripts(&[read(unrelated.summary()), unread]);
        assert!(usage.models.iter().any(|m| m.tokens.value().is_none()));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Unknown));

        // Read, then cut short, carrying a total the last session carries on.
        let first = read_for_run(
            &[
                stamped(prompt(), RUN_START_MS + 1),
                total(OPUS_100, RUN_START_MS + 1),
            ],
            Some(RUN_START_MS + 1),
        );
        let cut = read_for_run(
            &[
                stamped(prompt(), RUN_START_MS + 2),
                total(OPUS_100, RUN_START_MS + 1),
            ],
            Some(RUN_START_MS + 2),
        );
        let last = read_for_run(
            &[
                stamped(prompt(), RUN_START_MS + 3),
                total(OPUS_120, RUN_START_MS + 1),
            ],
            Some(RUN_START_MS + 3),
        );
        let usage = combine_transcripts(&[
            read(first.summary()),
            Observed {
                summary: Some(cut.summary()),
                read: false,
                cut_short: true,
            },
            read(last.summary()),
        ]);
        assert_eq!(opus_input(&usage), input(120));
        // The total's earliest transcript cut short: what it placed before
        // the run is not known.
        let usage = combine_transcripts(&[
            Observed {
                summary: Some(cut.summary()),
                read: false,
                cut_short: true,
            },
            read(last.summary()),
        ]);
        assert_eq!(usage.models, usage_unavailable(Unavailable::Incomplete));
    }
}
