//! PRD #1858: the deck's own usage file, for the agents whose usage reaches
//! the deck through code the deck installs in them — Pi (the deck's Pi
//! extension) and OpenCode (the deck's OpenCode plugin).
//!
//! Each of those producers already sees every reply's usage as the agent
//! hands it over (Pi's `message_end`, OpenCode's `message.updated`), which is
//! what the Pi extension's own telemetry sends as `pi.model_usage`. Rather
//! than have the deck find and parse each agent's private store, the producer
//! appends one line per reply, of named fields only, to a file whose path the
//! deck gives the run in [`USAGE_FILE_ENV`]. The deck reads it with
//! [`UsageFileReader`]. The format has no field a prompt, a reply, a tool's
//! input or output, an error message or a path could go in, so a producer
//! that writes what it should puts no content in the file.
//!
//! The format is the deck's, versioned by `v` ([`VERSION`]); a line of
//! another version, or one that does not parse, makes the run's usage
//! [`Unavailable::UnrecognizedFormat`] when nothing usable is beside it and
//! [`Unavailable::Incomplete`] when something is, since the reply it held is
//! not counted. A count the agent gave its producer that was missing or not a
//! count is written as `null`, so its reply is unrecognized rather than a
//! zero. A `turn_start` line, which the Pi extension writes when Pi starts
//! answering a prompt, says a turn began and adds no usage, so a Pi that
//! dies before its next reply ends mid-turn. The agents' own words for
//! how a reply stopped and why it failed are passed through raw (`stop`,
//! `error`) and mapped here, so a stop reason an agent upgrade adds becomes
//! `unknown`, never a wrong outcome.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    AgentTurnEnd, AgentUsage, Field, Ident, JsonlReader, LineSummary, MicroUsd, OptScalar, OptStr,
    ReaderStats, Tokens, Unavailable, UsageAcc, agent_cost, input_plus_cache, usage_unavailable,
};

/// The variable the deck sets in a Pi or OpenCode run's environment, naming
/// the file the producer appends to.
pub const USAGE_FILE_ENV: &str = "DOT_AGENT_DECK_USAGE_FILE";

/// The format version this reader reads.
pub const VERSION: u64 = 1;

/// The most replies with an `id` one run tracks. Past it, replies are no
/// longer counted and the run's usage is [`Unavailable::Incomplete`].
pub const MAX_TRACKED_REPLIES: usize = 65_536;

/// An incremental reader of one run's usage file.
pub type UsageFileReader = JsonlReader<UsageFileSummary>;

/// The agents that write a usage file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UsageAgent {
    Pi,
    OpenCode,
}

impl UsageAgent {
    /// The agent's name on a line (`agent`), as `deck.agent` spells it.
    pub const fn as_str(self) -> &'static str {
        match self {
            UsageAgent::Pi => "pi",
            UsageAgent::OpenCode => "opencode",
        }
    }

    fn parse(name: &str) -> Option<UsageAgent> {
        [UsageAgent::Pi, UsageAgent::OpenCode]
            .into_iter()
            .find(|a| a.as_str() == name)
    }
}

/// One reply, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    model: Option<Ident>,
    tokens: Field<Tokens>,
    cost: Field<MicroUsd>,
}

/// What one agent's lines in the file add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AgentLines {
    /// Replies without an `id`, each written once (Pi), folded as they come.
    anonymous: UsageAcc,
    /// Replies with an `id`, of which OpenCode writes a line per update:
    /// the last line for an id is the reply's usage.
    by_id: BTreeMap<Ident, Reply>,
    /// More than [`MAX_TRACKED_REPLIES`] ids, or an `id` that is not an
    /// identifier, so some replies are not counted.
    lost: bool,
    last_turn: Option<AgentTurnEnd>,
}

/// What a run's usage file holds, as [`UsageFileReader`] accumulates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageFileSummary {
    agents: BTreeMap<UsageAgent, AgentLines>,
    /// Lines of another version or type, or of an agent this reader does
    /// not know.
    pub unknown_lines: u64,
    /// A line with a marker did not parse.
    pub malformed: bool,
}

impl UsageFileSummary {
    /// `agent`'s usage in the file: lines naming another agent (a nested
    /// run of another agent inheriting the variable) are not counted. A file
    /// with no line for `agent` has its usage not recorded, or unrecognized
    /// when it held only lines this reader could not read.
    pub fn usage(&self, agent: UsageAgent) -> AgentUsage {
        let unrecognized = || {
            usage_unavailable(if self.unknown_lines > 0 || self.malformed {
                Unavailable::UnrecognizedFormat
            } else {
                Unavailable::NotRecorded
            })
        };
        let Some(lines) = self.agents.get(&agent) else {
            return AgentUsage {
                models: unrecognized(),
                quota: Field::Unavailable(Unavailable::NoChannel),
                last_turn: None,
            };
        };
        let models = if lines.lost {
            usage_unavailable(Unavailable::Incomplete)
        } else if self.unknown_lines > 0 || self.malformed {
            // A line the reader could not read may have been a reply of
            // this agent's: what it did read is a part, not the whole.
            if lines.anonymous.is_empty() && lines.by_id.is_empty() {
                unrecognized()
            } else {
                usage_unavailable(Unavailable::Incomplete)
            }
        } else {
            let mut acc = lines.anonymous.clone();
            for reply in lines.by_id.values() {
                acc.add(reply.model.clone(), reply.tokens, reply.cost);
            }
            if acc.is_empty() {
                unrecognized()
            } else {
                acc.finish()
            }
        };
        AgentUsage {
            models,
            quota: Field::Unavailable(Unavailable::NoChannel),
            last_turn: lines.last_turn.clone(),
        }
    }

    fn observe_line(&mut self, line: UsageLine) {
        let kind = line.kind.0.as_deref();
        if line.v.count() != Some(VERSION) || !matches!(kind, Some("reply" | "turn_start")) {
            self.unknown_lines += 1;
            return;
        }
        let Some(agent) = line.agent.0.as_deref().and_then(UsageAgent::parse) else {
            self.unknown_lines += 1;
            return;
        };
        if kind == Some("turn_start") {
            self.agents.entry(agent).or_default().last_turn = Some(AgentTurnEnd::MidTurn);
            return;
        }
        let reply = line.reply();
        let last_turn = line.turn_end(agent);
        let lines = self.agents.entry(agent).or_default();
        lines.last_turn = Some(last_turn);
        match line.id.0.as_deref().map(Ident::new) {
            None => {
                lines.anonymous.add(reply.model, reply.tokens, reply.cost);
            }
            Some(None) => lines.lost = true,
            Some(Some(id)) => {
                if lines.by_id.len() >= MAX_TRACKED_REPLIES && !lines.by_id.contains_key(&id) {
                    lines.lost = true;
                } else {
                    lines.by_id.insert(id, reply);
                }
            }
        }
    }
}

impl LineSummary for UsageFileSummary {
    const MARKERS: &'static [&'static [u8]] = &[b"\"type\":\"reply\"", b"\"type\":\"turn_start\""];
    type Record = UsageLine;

    fn observe(&mut self, record: UsageLine, _stats: &mut ReaderStats) {
        self.observe_line(record);
    }

    fn malformed(&mut self, _line: &[u8]) {
        self.malformed = true;
    }
}

/// One line of the usage file. Every field is lenient about its type, so an
/// unexpected value costs that field, not the line.
#[derive(Deserialize)]
pub struct UsageLine {
    #[serde(default)]
    v: OptScalar,
    #[serde(rename = "type", default)]
    kind: OptStr,
    #[serde(default)]
    agent: OptStr,
    /// The reply's id, for an agent that writes a reply more than once.
    #[serde(default)]
    id: OptStr,
    #[serde(default)]
    model: OptStr,
    /// Input tokens without the cache, as Pi and OpenCode count them.
    #[serde(default)]
    input: OptScalar,
    #[serde(default)]
    output: OptScalar,
    /// Left out for zero; present, it must be a count.
    #[serde(default)]
    cache_read: Present,
    #[serde(default)]
    cache_write: Present,
    /// The agent's own cost figure for the reply; absent when it has none.
    #[serde(default)]
    cost_usd: OptScalar,
    /// `false` when the provider returned no usage for the reply (a request
    /// that failed or never completed), whose counts are then the agent's
    /// initial zeros, not a count.
    #[serde(default)]
    reported: OptScalar,
    /// How the reply stopped, in the agent's own words.
    #[serde(default)]
    stop: OptStr,
    /// The kind of the reply's error, in the agent's own words: an error
    /// name or kind, never its message.
    #[serde(default)]
    error: OptStr,
    /// The HTTP status of the provider response that failed the reply.
    #[serde(default)]
    status: OptScalar,
}

impl UsageLine {
    fn reply(&self) -> Reply {
        let model = self.model.0.as_deref().and_then(Ident::new);
        let unrecorded = |reason| Reply {
            model: model.clone(),
            tokens: Field::Unavailable(reason),
            cost: Field::Unavailable(reason),
        };
        if self.reported.as_bool() == Some(false) {
            return unrecorded(Unavailable::NotRecorded);
        }
        let cache = |v: &Present| match &v.0 {
            None => Some(0),
            Some(v) => v.count(),
        };
        let (Some(input), Some(output), Some(cache_read), Some(cache_write)) = (
            self.input.count(),
            self.output.count(),
            cache(&self.cache_read),
            cache(&self.cache_write),
        ) else {
            return unrecorded(Unavailable::UnrecognizedFormat);
        };
        let tokens = input_plus_cache(input, output, cache_read, cache_write);
        let cost = match &self.cost_usd {
            OptScalar::Other => None,
            v => match v.amount() {
                Some(usd) => MicroUsd::from_usd(usd),
                None => return unrecorded(Unavailable::UnrecognizedFormat),
            },
        };
        Reply {
            model,
            tokens: Field::ProviderReported(tokens),
            cost: agent_cost(cost, &tokens),
        }
    }

    fn turn_end(&self, agent: UsageAgent) -> AgentTurnEnd {
        let stop = self.stop.0.as_deref();
        let error = self.error.0.as_deref();
        let status = self.status.http_status();
        match agent {
            UsageAgent::Pi => match stop {
                Some("stop" | "length") => AgentTurnEnd::Completed,
                Some("toolUse") => AgentTurnEnd::MidTurn,
                Some("aborted") => AgentTurnEnd::Interrupted,
                Some("error") => AgentTurnEnd::Failed { kind: None, status },
                _ => AgentTurnEnd::Unknown,
            },
            UsageAgent::OpenCode => match (error, stop) {
                (Some("MessageAbortedError"), _) => AgentTurnEnd::Interrupted,
                (Some(name), _) => AgentTurnEnd::Failed {
                    kind: Ident::new(opencode_error_kind(name)),
                    status,
                },
                (None, Some("stop" | "length")) => AgentTurnEnd::Completed,
                // No finish yet: the reply was still being written.
                (None, Some("tool-calls") | None) => AgentTurnEnd::MidTurn,
                (None, Some(_)) => AgentTurnEnd::Unknown,
            },
        }
    }
}

/// A field that may be left out, told apart from one given as `null` or as
/// a value of another type: `None` only when it is absent.
#[derive(Default)]
pub struct Present(Option<OptScalar>);

impl<'de> Deserialize<'de> for Present {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        OptScalar::deserialize(d).map(|v| Present(Some(v)))
    }
}

/// The deck's name for an OpenCode error type.
fn opencode_error_kind(name: &str) -> &'static str {
    match name {
        "APIError" => "api_error",
        "ProviderAuthError" => "provider_auth_error",
        "MessageOutputLengthError" => "output_length_exceeded",
        "ContextOverflowError" => "context_overflow",
        "UnknownError" => "unknown",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::super::Provenance;
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    const SENTINEL: &str = "SENTINEL9b2e41";

    fn pi(model: &str, input: u64, output: u64, cost: Option<f64>, stop: &str) -> String {
        let cost = cost.map_or(String::new(), |c| format!(r#","cost_usd":{c}"#));
        format!(
            r#"{{"v":1,"type":"reply","agent":"pi","model":"{model}","input":{input},"output":{output},"cache_read":5,"cache_write":1{cost},"stop":"{stop}"}}"#
        )
    }

    fn opencode(id: &str, input: u64, cost: f64, extra: &str) -> String {
        format!(
            r#"{{"v":1,"type":"reply","agent":"opencode","id":"{id}","model":"gpt-5.4","input":{input},"output":2,"cache_read":0,"cache_write":0,"cost_usd":{cost}{extra}}}"#
        )
    }

    fn read(lines: &[String]) -> UsageFileReader {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let mut reader = UsageFileReader::new(&path);
        reader.poll().unwrap();
        reader
    }

    /// Pi's replies are summed per model with the cache counted in the
    /// input, its cost kept as its estimate, and its last reply names the
    /// turn's end.
    #[test]
    fn pi_replies_sum_per_model() {
        let reader = read(&[
            pi("gpt-5.5", 10, 2, Some(0.5), "toolUse"),
            pi("gpt-5.5", 20, 3, Some(0.25), "stop"),
        ]);
        let usage = reader.summary().usage(UsageAgent::Pi);
        assert_eq!(usage.models.len(), 1);
        assert_eq!(
            usage.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 42,
                output: 5,
                cache_read: 10,
                cache_write: 2,
            })
        );
        assert_eq!(usage.models[0].cost, Field::Estimated(MicroUsd(750_000)));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Completed));
    }

    /// OpenCode writes a reply once per update: the last line for an id is
    /// its usage, not one more reply.
    #[test]
    fn opencode_updates_replace_their_reply() {
        let reader = read(&[
            opencode("msg_a", 0, 0.0, r#","reported":false"#),
            opencode("msg_a", 10, 0.1, r#","stop":"tool-calls""#),
            opencode("msg_b", 5, 0.1, r#","stop":"stop""#),
        ]);
        let usage = reader.summary().usage(UsageAgent::OpenCode);
        assert_eq!(
            usage.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 15,
                output: 4,
                cache_read: 0,
                cache_write: 0,
            })
        );
        assert_eq!(usage.models[0].cost, Field::Estimated(MicroUsd(200_000)));
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::Completed));
    }

    /// OpenCode's subscription zeros are unavailable, not free, and a reply
    /// the provider returned no usage for is not recorded, never zero.
    #[test]
    fn subscription_zeros_and_unreported_replies_are_unavailable() {
        let reader = read(&[opencode("msg_a", 10, 0.0, r#","stop":"stop""#)]);
        assert_eq!(
            reader.summary().usage(UsageAgent::OpenCode).models[0].cost,
            Field::Unavailable(Unavailable::SubscriptionZero)
        );
        let reader = read(&[opencode(
            "msg_a",
            0,
            0.0,
            r#","reported":false,"error":"APIError","status":400"#,
        )]);
        let usage = reader.summary().usage(UsageAgent::OpenCode);
        assert_eq!(
            usage.models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
        assert_eq!(
            usage.last_turn,
            Some(AgentTurnEnd::Failed {
                kind: Ident::new("api_error"),
                status: Some(400),
            })
        );
    }

    /// How each agent's reply stopped, mapped from its own words; a word
    /// the deck does not know is unknown, never a guess.
    #[test]
    fn each_agents_stop_words_map_to_a_turn_end() {
        let cases = [
            (
                pi("m", 1, 1, None, "aborted"),
                UsageAgent::Pi,
                AgentTurnEnd::Interrupted,
            ),
            (
                pi("m", 1, 1, None, "error"),
                UsageAgent::Pi,
                AgentTurnEnd::Failed {
                    kind: None,
                    status: None,
                },
            ),
            (
                pi("m", 1, 1, None, "length"),
                UsageAgent::Pi,
                AgentTurnEnd::Completed,
            ),
            (
                pi("m", 1, 1, None, "brandNew"),
                UsageAgent::Pi,
                AgentTurnEnd::Unknown,
            ),
            (
                opencode("x", 1, 1.0, r#","error":"MessageAbortedError""#),
                UsageAgent::OpenCode,
                AgentTurnEnd::Interrupted,
            ),
            (
                opencode("x", 1, 1.0, r#","error":"SomethingNew""#),
                UsageAgent::OpenCode,
                AgentTurnEnd::Failed {
                    kind: Ident::new("other"),
                    status: None,
                },
            ),
            (
                opencode("x", 1, 1.0, ""),
                UsageAgent::OpenCode,
                AgentTurnEnd::MidTurn,
            ),
            (
                opencode("x", 1, 1.0, r#","stop":"novel""#),
                UsageAgent::OpenCode,
                AgentTurnEnd::Unknown,
            ),
        ];
        for (line, agent, want) in cases {
            let reader = read(std::slice::from_ref(&line));
            assert_eq!(
                reader.summary().usage(agent).last_turn,
                Some(want),
                "{line}"
            );
        }
    }

    /// Lines of another agent — a nested run inheriting the variable — are
    /// not the run's.
    #[test]
    fn another_agents_lines_are_not_counted() {
        let reader = read(&[pi("gpt-5.5", 10, 2, Some(0.5), "stop")]);
        assert_eq!(
            reader.summary().usage(UsageAgent::OpenCode).models[0].tokens,
            Field::Unavailable(Unavailable::NotRecorded)
        );
    }

    /// A version or shape the reader does not know is unrecognized, never
    /// zero: another `v`, an unknown agent, counts that are not numbers, a
    /// line that does not parse.
    #[test]
    fn a_changed_format_is_unrecognized() {
        for line in [
            pi("gpt-5.5", 10, 2, None, "stop").replace(r#""v":1"#, r#""v":2"#),
            pi("gpt-5.5", 10, 2, None, "stop").replace(r#""agent":"pi""#, r#""agent":"zed""#),
            r#"{"v":1,"type":"reply","agent":"pi","#.to_owned(),
        ] {
            let reader = read(std::slice::from_ref(&line));
            assert_eq!(
                reader.summary().usage(UsageAgent::Pi).models[0].tokens,
                Field::Unavailable(Unavailable::UnrecognizedFormat),
                "{line}"
            );
        }
        let reader =
            read(
                &[pi("gpt-5.5", 10, 2, None, "stop").replace(r#""input":10"#, r#""input":"ten""#)],
            );
        assert_eq!(
            reader.summary().usage(UsageAgent::Pi).models[0].tokens,
            Field::Unavailable(Unavailable::UnrecognizedFormat)
        );
    }

    /// Past the tracked-reply bound, or with an id that is not an
    /// identifier, some replies are not counted: incomplete.
    #[test]
    fn untracked_replies_make_the_usage_incomplete() {
        let reader = read(&[opencode(&format!("{SENTINEL} id"), 1, 1.0, "")]);
        let usage = reader.summary().usage(UsageAgent::OpenCode);
        assert_eq!(
            usage.models[0].tokens,
            Field::Unavailable(Unavailable::Incomplete)
        );
        assert!(!format!("{:?}", reader.summary()).contains(SENTINEL));
    }

    /// Strings that do not belong in the file — a sentinel in every string
    /// field, a hostile model name, an extra field carrying content — reach
    /// neither the summary nor its usage.
    #[test]
    fn content_never_leaves_the_reader() {
        let line = format!(
            r#"{{"v":1,"type":"reply","agent":"opencode","id":"msg_1","model":"{SENTINEL} model","input":1,"output":1,"cost_usd":0.1,"stop":"{SENTINEL}","error":"{SENTINEL}","prompt":"{SENTINEL}","text":"{SENTINEL}"}}"#
        );
        let reader = read(&[line]);
        let usage = reader.summary().usage(UsageAgent::OpenCode);
        assert!(!format!("{:?}", reader.summary()).contains(SENTINEL));
        assert!(!format!("{usage:?}").contains(SENTINEL));
        assert_eq!(usage.models[0].model, None);
        assert_eq!(
            usage.models[0].tokens.provenance(),
            Provenance::ProviderReported
        );
    }

    /// The Pi extension's lines for each case in its shared fixture (PRD
    /// #1858), read here: what the extension writes for a missing, renamed,
    /// string, negative or fractional count is unrecognized, never a
    /// provider-reported zero, and its initial zeros on a failed or aborted
    /// reply are not recorded. The extension's own test pins the same lines
    /// to the replies they come from (`pi-extension/test/usage-file.test.ts`).
    #[test]
    fn the_pi_extensions_lines_read_as_the_extension_means_them() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../pi-extension/test/usage-file-cases.json"
        ))
        .unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let reader = read(&[case["line"].to_string()]);
            let usage = reader.summary().usage(UsageAgent::Pi);
            let deck = &case["deck"];
            match deck["unavailable"].as_str() {
                Some(reason) => assert_eq!(
                    usage.models[0]
                        .tokens
                        .unavailable()
                        .map(Unavailable::as_str),
                    Some(reason),
                    "{name}"
                ),
                None => {
                    let want: Vec<u64> = deck["tokens"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v.as_u64().unwrap())
                        .collect();
                    let Field::ProviderReported(t) = usage.models[0].tokens else {
                        panic!("{name}: {:?}", usage.models[0].tokens);
                    };
                    assert_eq!(
                        vec![t.input, t.output, t.cache_read, t.cache_write],
                        want,
                        "{name}"
                    );
                }
            }
            assert_eq!(
                usage.last_turn.as_ref().map(AgentTurnEnd::as_str),
                deck["last_turn"].as_str(),
                "{name}"
            );
        }
    }

    /// A cache count left out is zero; given as `null`, a string or a
    /// negative number, it is not a count, and the reply is unrecognized.
    #[test]
    fn a_cache_count_left_out_is_zero_but_an_invalid_one_is_not() {
        let line = |cache: &str| {
            format!(
                r#"{{"v":1,"type":"reply","agent":"opencode","id":"m","model":"gpt-5.4","input":3,"output":2{cache},"stop":"stop"}}"#
            )
        };
        let reader = read(&[line("")]);
        assert_eq!(
            reader.summary().usage(UsageAgent::OpenCode).models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 3,
                output: 2,
                cache_read: 0,
                cache_write: 0,
            })
        );
        for cache in [
            r#","cache_read":null"#,
            r#","cache_write":"1""#,
            r#","cache_read":-1"#,
        ] {
            let reader = read(&[line(cache)]);
            assert_eq!(
                reader.summary().usage(UsageAgent::OpenCode).models[0].tokens,
                Field::Unavailable(Unavailable::UnrecognizedFormat),
                "{cache}"
            );
        }
    }

    /// A line the reader cannot read after replies it could — one that does
    /// not parse, or of another version — may have been a reply, so the
    /// replies read are a part: incomplete, not the earlier total.
    #[test]
    fn an_unreadable_line_after_usable_ones_makes_the_usage_incomplete() {
        for later in [
            r#"{"v":1,"type":"reply","agent":"pi","#.to_owned(),
            pi("gpt-5.5", 10, 2, None, "stop").replace(r#""v":1"#, r#""v":2"#),
        ] {
            let reader = read(&[pi("gpt-5.5", 10, 2, Some(0.5), "stop"), later.clone()]);
            assert_eq!(
                reader.summary().usage(UsageAgent::Pi).models,
                usage_unavailable(Unavailable::Incomplete),
                "{later}"
            );
        }
    }

    /// A turn's start adds no usage and makes the last turn mid-turn until a
    /// reply ends it.
    #[test]
    fn a_turn_start_marks_the_turn_open() {
        let start = r#"{"v":1,"type":"turn_start","agent":"pi"}"#.to_owned();
        let reader = read(&[pi("gpt-5.5", 10, 2, Some(0.5), "stop"), start.clone()]);
        let usage = reader.summary().usage(UsageAgent::Pi);
        assert_eq!(usage.last_turn, Some(AgentTurnEnd::MidTurn));
        assert_eq!(
            usage.models[0].tokens,
            Field::ProviderReported(Tokens {
                input: 16,
                output: 2,
                cache_read: 5,
                cache_write: 1,
            })
        );
        let reader = read(&[start, pi("gpt-5.5", 10, 2, Some(0.5), "stop")]);
        assert_eq!(
            reader.summary().usage(UsageAgent::Pi).last_turn,
            Some(AgentTurnEnd::Completed)
        );
    }
}
