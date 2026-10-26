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
//! not this one's: the deck marks it as the baseline
//! ([`ClaudeTranscriptSummary::seal_baseline`]) and counts only what the
//! total grew by since. A total that went down in any count since the
//! baseline cannot be told apart from a counter that restarted, so it is
//! [`Unavailable::Incomplete`], never a guess.

use std::collections::BTreeMap;
use std::fmt;

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
}

impl ClaudeTranscriptSummary {
    /// Mark everything read so far as the transcript's state when the run
    /// began: its last `cost-state` becomes the baseline the run's usage is
    /// counted from, and its last turn is forgotten. Called once, after the
    /// reader has first caught up with a transcript that was already there
    /// when the run began (a resumed session); a transcript the run created
    /// holds only the run's own records and needs no baseline. A second call
    /// does nothing.
    pub fn seal_baseline(&mut self) {
        if self.sealed {
            return;
        }
        self.sealed = true;
        self.baseline = match &self.latest {
            Some(Ok(state)) => Some(state.clone()),
            _ => None,
        };
        self.last_turn = None;
        self.before_user = None;
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

    /// The run's usage. Before [`seal_baseline`](Self::seal_baseline) every
    /// total read counts as the run's own.
    pub fn usage(&self) -> AgentUsage {
        let models = match (&self.latest, self.sealed && !self.total_since_seal) {
            (None, _) | (_, true) => usage_unavailable(Unavailable::NotRecorded),
            (Some(Err(())), _) => usage_unavailable(Unavailable::UnrecognizedFormat),
            (Some(Ok(latest)), _) => self.models_since_baseline(latest),
        };
        AgentUsage {
            models,
            quota: Field::Unavailable(Unavailable::NoChannel),
            last_turn: self.last_turn.clone(),
        }
    }

    fn models_since_baseline(&self, latest: &CostState) -> Vec<super::ModelUsage> {
        let empty = CostState::default();
        let base = self.baseline.as_ref().unwrap_or(&empty);
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

    fn observe_record(&mut self, record: ClaudeRecord) {
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

/// One run's usage from the transcripts of every Claude Code session it
/// held, in the order the run saw them. Claude Code can carry one running
/// total from a session into the next (a `/clear`, or a resume), so of the
/// transcripts whose last totals share a start, only the last is counted;
/// the rest are combined as separate sessions
/// ([`super::combine_sessions`]).
pub fn combine_transcripts(sessions: &[&ClaudeTranscriptSummary]) -> AgentUsage {
    let parts = sessions
        .iter()
        .enumerate()
        .filter(|(i, session)| {
            session.tracker_start().is_none_or(|start| {
                !sessions[i + 1..]
                    .iter()
                    .any(|later| later.tracker_start() == Some(start))
            })
        })
        .map(|(_, session)| session.usage())
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

    fn malformed(&mut self, line: &[u8]) {
        // A total that did not parse is newer than the last one read, so
        // keeping the older one would report a figure the session has since
        // moved past: it is unrecognized instead.
        if super::line_is_candidate(line, &[Self::MARKERS[0]]) {
            self.latest = Some(Err(()));
            if self.sealed {
                self.total_since_seal = true;
            }
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
/// is not an object of objects each with an input and an output count, so a
/// changed format is unreadable rather than read as zero. Past
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
                    let cache = |v: &OptScalar| match v {
                        OptScalar::Other => Some(0),
                        v => v.count(),
                    };
                    let (Some(cache_read), Some(cache_write)) =
                        (cache(&entry.cache_read), cache(&entry.cache_write))
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
        let carried = combine_transcripts(&[first.summary(), second.summary()]);
        assert_eq!(
            model(&carried, "claude-opus-5-5").tokens,
            Field::ProviderReported(Tokens {
                input: 15,
                output: 30,
                cache_read: 0,
                cache_write: 0,
            })
        );
        let separate = combine_transcripts(&[first.summary(), other.summary()]);
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
    /// count that is not a number, `modelUsage` that is not an object — is
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
}
