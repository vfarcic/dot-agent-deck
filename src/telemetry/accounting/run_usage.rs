//! PRD #1858: one live run's usage readers, from the run's start to its end.
//!
//! The run tracker holds one [`RunUsage`] per open run. It learns where the
//! run's usage record is from what the deck already knows — the directories
//! an agent keeps its records in ([`UsageDirs`], resolved as the run is
//! prepared), the session id and working directory on Claude Code's events,
//! the rollout path on Codex's — opens a reader once the record is there,
//! polls it, and when the run ends drains it and turns what it read into the
//! run's record ([`RunUsage::finish`]).
//!
//! A [`RunUsage`] is a shared handle, so the reading happens outside the
//! tracker's lock: under it, [`RunUsage::observe`] only notes a session in a
//! list of its own, and the tracker hands out handles for the exporter to
//! [`poll`](RunUsage::poll) and [`finish`](RunUsage::finish) after it has let
//! go. A poll and a finish of one run wait for each other, never for the
//! tracker.
//!
//! Nothing here logs, and nothing it holds reaches a span but the record's
//! export ([`super::run_attributes`]): a path, a session id or a working
//! directory stays in this struct and is dropped with it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::event::{AgentEvent, AgentType};

use super::claude::{ClaudeTranscriptReader, Observed, combine_transcripts};
use super::locate::{self, TrustedDir};
use super::usage_file::{UsageAgent, UsageFileReader, UsageFileSummary};
use super::{
    AgentTurnEnd, AgentUsage, CodexUsageReader, Field, Ident, JsonlReader, LineSummary, RunExport,
    RunIdentity, RunRecord, Unavailable, UsageSource, combine_sessions, command_termination,
    from_claude, from_codex, no_usage_channel, run_attributes, usage_source, usage_unavailable,
};
use crate::codex_rollout_tail::CODEX_TRANSCRIPT_PATH_METADATA_KEY;
use crate::telemetry::tracker::EndReason;

/// The most agent sessions — Claude Code transcripts, Codex rollouts — one
/// run reads. A session past it is not read, and the run's usage is
/// [`Unavailable::Incomplete`].
pub const MAX_SESSIONS: usize = 16;

/// How many polls draining a run at its end may take, at most, each within
/// the reader's own byte budget. A record with more left unread than that is
/// [`Unavailable::Incomplete`], not the part that was read.
pub const MAX_DRAIN_POLLS: usize = 16;

/// Where a run's agent keeps its usage record, resolved from the run's
/// environment as it is prepared.
#[derive(Debug, Clone, Default)]
pub struct UsageDirs {
    /// Claude Code's configuration directory ([`locate::claude_config_dir`]),
    /// opened as a [`TrustedDir`] once it is there.
    pub claude_config: Option<PathBuf>,
    /// Codex's home ([`locate::codex_home`]); its `sessions` directory is
    /// opened as a [`TrustedDir`] once it is there.
    pub codex_home: Option<PathBuf>,
    /// The deck's usage file for the run, which the deck removes once the
    /// run has been read.
    pub usage_file: Option<UsageFile>,
}

/// A run's usage file: `name` in the deck's usage directory `dir`.
#[derive(Debug, Clone)]
pub struct UsageFile {
    pub dir: TrustedDir,
    pub name: String,
}

/// The sessions a run's events named, in the order it saw them. Kept apart
/// from the readers so that noting one never waits for a read.
#[derive(Default)]
struct Inbox {
    /// Claude Code: session id, working directory, and when the deck first
    /// saw it, in milliseconds since the Unix epoch.
    claude: Vec<(String, Option<String>, i64)>,
    /// Codex: the rollout path its hook reported.
    codex: Vec<String>,
    /// More sessions than [`MAX_SESSIONS`] were seen.
    lost: bool,
}

/// One session's reader, once its record is located (and admitted).
struct Session<S> {
    reader: Option<JsonlReader<S>>,
    /// The last poll could not open or read the record.
    failed: bool,
}

impl<S> Default for Session<S> {
    fn default() -> Self {
        Session {
            reader: None,
            failed: false,
        }
    }
}

impl<S: LineSummary> Session<S> {
    fn poll(&mut self) {
        if let Some(reader) = &mut self.reader {
            self.failed = reader.poll().is_err();
        }
    }

    /// Whether the record was read: found, and readable at the last poll.
    fn read(&self) -> Option<&JsonlReader<S>> {
        self.reader.as_ref().filter(|_| !self.failed)
    }

    /// Whether the record was read from, then could not be: what was read
    /// is a part of it.
    fn cut_short(&self) -> bool {
        self.failed
            && self
                .reader
                .as_ref()
                .is_some_and(|r| r.stats().bytes_read > 0)
    }
}

#[derive(Default)]
struct Readers {
    claude_root: Option<TrustedDir>,
    codex_sessions: Option<TrustedDir>,
    claude: Vec<Session<super::claude::ClaudeTranscriptSummary>>,
    codex: Vec<Session<super::CodexRolloutSummary>>,
    usage_file: Session<UsageFileSummary>,
}

struct Inner {
    source: UsageSource,
    agent: AgentType,
    dirs: UsageDirs,
    /// When the run began, in nanoseconds since the Unix epoch.
    started: u64,
    inbox: Mutex<Inbox>,
    readers: Mutex<Readers>,
}

/// See the module docs. Cheap to clone: a clone is the same run's.
#[derive(Clone)]
pub struct RunUsage {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The part of a run's usage that a session whose record was observed but
/// not read stands for — unknown, never nothing: not recorded, or
/// incomplete when the record was read from before it could not be.
fn unread_session(cut_short: bool) -> AgentUsage {
    let reason = if cut_short {
        Unavailable::Incomplete
    } else {
        Unavailable::NotRecorded
    };
    AgentUsage {
        models: usage_unavailable(reason),
        quota: Field::Unavailable(reason),
        last_turn: Some(AgentTurnEnd::Unknown),
    }
}

impl RunUsage {
    pub fn new(agent: &AgentType, dirs: UsageDirs, started: u64) -> Self {
        let source = usage_source(agent);
        let mut readers = Readers::default();
        if source == UsageSource::UsageFile
            && let Some(file) = &dirs.usage_file
        {
            readers.usage_file.reader = Some(UsageFileReader::beneath(
                file.dir.clone(),
                PathBuf::from(&file.name),
            ));
        }
        RunUsage {
            inner: Arc::new(Inner {
                source,
                agent: agent.clone(),
                dirs,
                started,
                inbox: Mutex::new(Inbox::default()),
                readers: Mutex::new(readers),
            }),
        }
    }

    /// An event of the run: a Claude Code event names its session and
    /// working directory, a Codex event its rollout. Reads nothing.
    pub fn observe(&self, event: &AgentEvent) {
        let inner = &self.inner;
        match inner.source {
            UsageSource::ClaudeTranscript if event.agent_type == AgentType::ClaudeCode => {
                let id = event.session_id.as_str();
                if !locate::admissible_session_id(id) {
                    return;
                }
                let mut inbox = lock(&inner.inbox);
                if inbox.claude.iter().any(|(s, _, _)| s == id) {
                    return;
                }
                if inbox.claude.len() >= MAX_SESSIONS {
                    inbox.lost = true;
                    return;
                }
                let seen = i64::try_from(crate::telemetry::now_unix_nanos() / 1_000_000)
                    .unwrap_or(i64::MAX);
                inbox.claude.push((id.to_string(), event.cwd.clone(), seen));
            }
            UsageSource::CodexRollout => {
                let Some(reported) = event.metadata.get(CODEX_TRANSCRIPT_PATH_METADATA_KEY) else {
                    return;
                };
                let mut inbox = lock(&inner.inbox);
                if inbox.codex.iter().any(|s| s == reported) {
                    return;
                }
                if inbox.codex.len() >= MAX_SESSIONS {
                    inbox.lost = true;
                    return;
                }
                inbox.codex.push(reported.clone());
            }
            _ => {}
        }
    }

    /// Read what each record gained since the last poll, locating the ones
    /// not found yet where their directory name says. Blocks on file reads:
    /// call it outside the tracker's lock.
    pub fn poll(&self) {
        let mut readers = lock(&self.inner.readers);
        self.poll_with(&mut readers, false);
    }

    fn poll_with(&self, readers: &mut Readers, search: bool) {
        let inner = &*self.inner;
        let (claude, codex) = {
            let inbox = lock(&inner.inbox);
            (
                inbox.claude[readers.claude.len()..].to_vec(),
                inbox.codex[readers.codex.len()..].to_vec(),
            )
        };
        readers
            .claude
            .extend(claude.iter().map(|_| Session::default()));
        readers
            .codex
            .extend(codex.iter().map(|_| Session::default()));
        if let Some(config) = inner.dirs.claude_config.as_deref() {
            if readers.claude_root.is_none() && !readers.claude.is_empty() {
                readers.claude_root = TrustedDir::open(config);
            }
            let inbox = lock(&inner.inbox);
            let observed = inbox.claude.clone();
            drop(inbox);
            if let Some(root) = &readers.claude_root {
                for (s, (id, cwd, _)) in readers.claude.iter_mut().zip(&observed) {
                    if s.reader.is_none()
                        && let Some(rel) =
                            locate::claude_transcript(root, cwd.as_deref(), id, search)
                    {
                        let mut reader = ClaudeTranscriptReader::beneath(root.clone(), rel);
                        reader.summary_mut().set_run_start(inner.started);
                        s.reader = Some(reader);
                    }
                    s.poll();
                    if let Some(reader) = &mut s.reader {
                        let born = reader.born();
                        reader.summary_mut().note_birth(born);
                    }
                }
            }
        }
        if let Some(home) = inner.dirs.codex_home.as_deref() {
            let sessions = home.join("sessions");
            if readers.codex_sessions.is_none() && !readers.codex.is_empty() {
                readers.codex_sessions = TrustedDir::open(&sessions);
            }
            let reported = lock(&inner.inbox).codex.clone();
            if let Some(root) = &readers.codex_sessions {
                for (s, reported) in readers.codex.iter_mut().zip(&reported) {
                    if s.reader.is_none()
                        && let Some(rel) = locate::admit_codex_rollout(reported, &sessions, root)
                    {
                        // A rollout is one Codex session's, and a resumed one
                        // is a new rollout, so it has no baseline to seal.
                        s.reader = Some(CodexUsageReader::beneath(root.clone(), rel));
                    }
                    s.poll();
                }
            }
        }
        readers.usage_file.poll();
    }

    /// The run's record, once it has ended as `end` (with `exit_code`, when
    /// the deck has one): every reader drained first, at most
    /// [`MAX_DRAIN_POLLS`] polls each. The deck's usage file is removed.
    /// Blocks on file reads: call it outside the tracker's lock.
    pub fn finish(self, run_id: &str, end: EndReason, exit_code: Option<i32>) -> RunExport {
        let inner = &*self.inner;
        let mut readers = lock(&inner.readers);
        self.poll_with(&mut readers, true);
        for _ in 1..MAX_DRAIN_POLLS {
            if !readers.backlogged() {
                break;
            }
            self.poll_with(&mut readers, false);
        }
        let mut usage = self.usage(&readers);
        let lost = lock(&inner.inbox).lost;
        if readers.backlogged() || lost || readers.unfinished() {
            usage.models = usage_unavailable(Unavailable::Incomplete);
        }
        if readers.unfinished() {
            // The agent's last record was cut off: it may have been the turn.
            usage.last_turn = Some(AgentTurnEnd::Unknown);
        }
        if let Some(file) = &inner.dirs.usage_file {
            file.dir.remove(Path::new(&file.name).as_os_str());
        }
        drop(readers);
        let agent = crate::telemetry::tracker::agent_name(&inner.agent);
        let identity = RunIdentity::new(
            Ident::new(run_id).unwrap_or_else(|| Ident::new("unknown").expect("an identifier")),
            Ident::new(agent).expect("an agent name is an identifier"),
        );
        let mut record = RunRecord::new(identity, usage, Some(end));
        if inner.agent == AgentType::None {
            record.termination = command_termination(Some(end), exit_code);
        }
        run_attributes(&record)
    }

    /// The agent's usage from what the readers read. A session whose record
    /// was observed but could not be read stands for an unknown part of the
    /// run, which makes its total incomplete, unless a later Claude Code
    /// transcript's running total started before the deck saw that session,
    /// and so covers it. With no session read at all, the usage is not
    /// recorded.
    fn usage(&self, readers: &Readers) -> AgentUsage {
        let inner = &*self.inner;
        match inner.source {
            UsageSource::NoChannel => no_usage_channel(),
            UsageSource::ClaudeTranscript => {
                if readers.claude.is_empty() {
                    return from_claude(None);
                }
                let seen = lock(&inner.inbox).claude.clone();
                let observed: Vec<Observed> = readers
                    .claude
                    .iter()
                    .zip(&seen)
                    .map(|(s, (_, _, at))| Observed {
                        summary: s.read().map(JsonlReader::summary),
                        cut_short: s.cut_short(),
                        seen_at_ms: *at,
                    })
                    .collect();
                combine_transcripts(&observed)
            }
            UsageSource::CodexRollout => {
                if readers.codex.is_empty() {
                    return from_codex(&super::CodexRolloutSummary::default());
                }
                let unread = readers.codex.iter().any(|s| s.read().is_none());
                let mut usage = combine_sessions(
                    readers
                        .codex
                        .iter()
                        .map(|s| {
                            s.read().map_or_else(
                                || unread_session(s.cut_short()),
                                |r| from_codex(r.summary()),
                            )
                        })
                        .collect(),
                );
                if unread && usage.quota.value().is_some() {
                    // The unread rollout's samples of the window are missing.
                    usage.quota = Field::Unavailable(Unavailable::Incomplete);
                }
                usage
            }
            UsageSource::UsageFile => {
                let agent = match inner.agent {
                    AgentType::OpenCode => UsageAgent::OpenCode,
                    _ => UsageAgent::Pi,
                };
                let file = &readers.usage_file;
                let mut usage = match &file.reader {
                    Some(reader) => reader.summary().usage(agent),
                    None => UsageFileSummary::default().usage(agent),
                };
                let read_any = file
                    .reader
                    .as_ref()
                    .is_some_and(|r| r.stats().bytes_read > 0);
                if file.failed && read_any {
                    // It was read, then could not be: the rest is unknown.
                    usage.models = usage_unavailable(Unavailable::Incomplete);
                    usage.last_turn = Some(AgentTurnEnd::Unknown);
                }
                usage
            }
        }
    }
}

impl Readers {
    fn readers(&self) -> impl Iterator<Item = (u64, bool)> + '_ {
        let claude = self.claude.iter().filter_map(|s| s.reader.as_ref());
        let codex = self.codex.iter().filter_map(|s| s.reader.as_ref());
        claude
            .map(|r| (r.stats().backlog, r.unfinished_candidate()))
            .chain(codex.map(|r| (r.stats().backlog, r.unfinished_candidate())))
            .chain(
                self.usage_file
                    .reader
                    .iter()
                    .map(|r| (r.stats().backlog, r.unfinished_candidate())),
            )
    }

    fn backlogged(&self) -> bool {
        self.readers().any(|(backlog, _)| backlog > 0)
    }

    /// A reader holds the start of a record whose line never ended.
    fn unfinished(&self) -> bool {
        self.readers().any(|(_, unfinished)| unfinished)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventType;
    use crate::telemetry::span::AttrValue;
    use std::collections::HashMap;
    use std::io::Write as _;

    fn exported(export: &RunExport) -> HashMap<&'static str, AttrValue> {
        export
            .run
            .iter()
            .map(|a| (a.key().as_str(), a.value().clone()))
            .collect()
    }

    fn int(n: i64) -> AttrValue {
        AttrValue::Int(n)
    }

    fn text(s: &str) -> AttrValue {
        AttrValue::Str(s.to_string())
    }

    fn claude_event(session: &str, cwd: &str) -> AgentEvent {
        let mut event = crate::telemetry::tracker::tests::event("p", EventType::Thinking);
        event.session_id = session.to_string();
        event.cwd = Some(cwd.to_string());
        event
    }

    fn codex_event(path: &Path) -> AgentEvent {
        let mut event = crate::telemetry::tracker::tests::event("p", EventType::Thinking);
        event.metadata.insert(
            CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
            path.to_string_lossy().into_owned(),
        );
        event
    }

    fn append(path: &Path, lines: &[&str]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn claude_dirs(config: &Path) -> UsageDirs {
        UsageDirs {
            claude_config: Some(config.to_path_buf()),
            ..UsageDirs::default()
        }
    }

    fn codex_dirs(home: &Path) -> UsageDirs {
        UsageDirs {
            codex_home: Some(home.to_path_buf()),
            ..UsageDirs::default()
        }
    }

    fn usage_file_dirs(dir: &Path, name: &str) -> UsageDirs {
        UsageDirs {
            usage_file: Some(UsageFile {
                dir: TrustedDir::open(dir).unwrap(),
                name: name.to_string(),
            }),
            ..UsageDirs::default()
        }
    }

    const COST_STATE: &str = r#"{"type":"cost-state","totalCostUSD":0.0123,"startTime":1790000000000,"modelUsage":{"claude-haiku-5-5":{"inputTokens":10,"outputTokens":20,"cacheReadInputTokens":300,"cacheCreationInputTokens":40,"costUSD":0.0123}},"hasUnknownModelCost":false}"#;

    /// A cost-state as [`COST_STATE`], with another input count, cost and
    /// total start.
    fn cost_state(input: u64, cost: &str, start_ms: i64) -> String {
        COST_STATE
            .replace(r#""inputTokens":10"#, &format!(r#""inputTokens":{input}"#))
            .replace(r#""costUSD":0.0123"#, &format!(r#""costUSD":{cost}"#))
            .replace("1790000000000", &start_ms.to_string())
    }

    fn stamped(kind: &str, at_ms: i64) -> String {
        let at = chrono::DateTime::from_timestamp_millis(at_ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        match kind {
            "user" => format!(
                r#"{{"type":"user","timestamp":"{at}","message":{{"role":"user","content":"hi"}}}}"#
            ),
            _ => format!(
                r#"{{"type":"assistant","timestamp":"{at}","message":{{"model":"claude-haiku-5-5","stop_reason":"end_turn","content":[]}}}}"#
            ),
        }
    }

    fn now_ms() -> i64 {
        i64::try_from(crate::telemetry::now_unix_nanos() / 1_000_000).unwrap()
    }

    const CODEX_TOTAL: &str = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}}}"#;

    /// A Claude Code run reads the transcript of the session its events
    /// name, even one written after the event that named it, and drains it
    /// at the run's end.
    #[test]
    fn a_claude_run_reads_the_transcript_its_events_name() {
        let config = tempfile::tempdir().unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        usage.observe(&claude_event("s-1", "/work/x.y"));
        usage.poll();
        append(
            &config.path().join("projects/-work-x-y/s-1.jsonl"),
            &[
                r#"{"type":"user","message":{"role":"user","content":"hello"}}"#,
                r#"{"type":"assistant","message":{"model":"claude-haiku-5-5","stop_reason":"end_turn","content":[]}}"#,
                COST_STATE,
            ],
        );
        let run = exported(&usage.finish("run1", EndReason::Exited, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(350));
        assert_eq!(run["gen_ai.usage.output_tokens"], int(20));
        assert_eq!(run["deck.tokens.provenance"], text("provider_reported"));
        assert_eq!(run["deck.cost.microusd"], int(12300));
        assert_eq!(run["deck.run.termination"], text("completed"));
        assert_eq!(run["deck.quota.unavailable_reason"], text("no_channel"));
    }

    /// The earlier run's part of a resumed session's transcript.
    fn earlier_run(path: &Path, before: i64, total_start: i64) {
        append(
            path,
            &[
                &stamped("user", before),
                &stamped("assistant", before + 1),
                &cost_state(10, "0.0123", total_start),
            ],
        );
    }

    /// A resumed session's transcript credits the run only with what its
    /// total grew by since its last total before the run, however much the
    /// run had written by the time the deck first read it: here the new
    /// prompt, reply and total are all there before the first poll.
    #[test]
    fn a_resumed_transcript_counts_from_its_boundary_not_the_first_poll() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("projects/-w/s-2.jsonl");
        let started = crate::telemetry::now_unix_nanos();
        let start_ms = now_ms();
        earlier_run(&path, start_ms - 60_000, start_ms - 120_000);
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), started);
        usage.observe(&claude_event("s-2", "/w"));
        append(
            &path,
            &[
                &stamped("user", start_ms + 1),
                &stamped("assistant", start_ms + 2),
                &cost_state(15, "0.0200", start_ms - 120_000),
            ],
        );
        usage.poll();
        let run = exported(&usage.finish("run2", EndReason::Exited, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(5));
        assert_eq!(run["gen_ai.usage.output_tokens"], int(0));
        assert_eq!(run["deck.cost.microusd"], int(7700));
        assert_eq!(run["deck.run.termination"], text("completed"));
    }

    /// A resumed transcript first found by the search at the run's end — its
    /// session reported from a working directory other than its own — is
    /// counted from the same boundary.
    #[test]
    fn a_resumed_transcript_found_at_the_end_counts_from_its_boundary() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("projects/-real-dir/s-3.jsonl");
        let started = crate::telemetry::now_unix_nanos();
        let start_ms = now_ms();
        earlier_run(&path, start_ms - 60_000, start_ms - 120_000);
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), started);
        usage.observe(&claude_event("s-3", "/reported/elsewhere"));
        usage.poll();
        append(
            &path,
            &[
                &stamped("user", start_ms + 1),
                &cost_state(14, "0.0200", start_ms - 120_000),
            ],
        );
        let run = exported(&usage.finish("run3", EndReason::Exited, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(4));
        assert_eq!(run["deck.cost.microusd"], int(7700));
        assert_eq!(run["deck.run.last_turn"], text("mid_turn"));
    }

    /// In a resumed transcript, a total that started during the run is the
    /// run's alone, and one carried from before the run with no total before
    /// the boundary to count from is incomplete.
    #[test]
    fn a_resumed_transcripts_total_is_the_runs_only_by_its_start() {
        let config = tempfile::tempdir().unwrap();
        for (name, total_start, earlier_total, want) in [
            ("s-own", 1, true, Some(350)),
            ("s-carried", -120_000, false, None),
        ] {
            let path = config.path().join(format!("projects/-w/{name}.jsonl"));
            let started = crate::telemetry::now_unix_nanos();
            let start_ms = now_ms();
            if earlier_total {
                earlier_run(&path, start_ms - 60_000, start_ms - 120_000);
            } else {
                append(&path, &[&stamped("user", start_ms - 60_000)]);
            }
            let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), started);
            usage.observe(&claude_event(name, "/w"));
            append(
                &path,
                &[
                    &stamped("user", start_ms + 1),
                    &cost_state(10, "0.0123", start_ms + total_start),
                ],
            );
            let run = exported(&usage.finish(name, EndReason::Exited, None));
            match want {
                Some(input) => assert_eq!(run["gen_ai.usage.input_tokens"], int(input), "{name}"),
                None => {
                    assert_eq!(
                        run["deck.tokens.unavailable_reason"],
                        text("incomplete"),
                        "{name}"
                    );
                    assert!(!run.contains_key("gen_ai.usage.input_tokens"));
                }
            }
        }
    }

    /// A transcript that was there before the run began, holding a total but
    /// no stamped record to place the run's start by, is incomplete, not the
    /// earlier run's total.
    #[test]
    fn an_older_transcript_with_no_boundary_is_incomplete() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("projects/-w/s-4.jsonl");
        append(&path, &[COST_STATE]);
        let probe = std::fs::metadata(&path).unwrap();
        if probe.created().is_err() {
            eprintln!("SKIP: this file system records no creation time");
            return;
        }
        let started = crate::telemetry::now_unix_nanos() + 1_000_000_000;
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), started);
        usage.observe(&claude_event("s-4", "/w"));
        append(&path, &[&cost_state(15, "0.0200", 1_790_000_000_000)]);
        let run = exported(&usage.finish("run4", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
    }

    /// Make `path` unreadable to this user; `false` (skip) when running as
    /// root, which reads it anyway.
    #[cfg(unix)]
    fn unreadable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        if super::super::effective_uid() == 0 {
            eprintln!("SKIP: root reads an unreadable file");
            return false;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        true
    }

    /// A second Claude Code session whose transcript is missing, or there but
    /// unreadable, is an unknown part of the run: its total is incomplete and
    /// its last turn unknown, never the first session's figure and turn.
    #[test]
    fn a_claude_session_not_read_makes_the_run_incomplete() {
        for unreadable_second in [false, true] {
            let config = tempfile::tempdir().unwrap();
            let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
            usage.observe(&claude_event("s-a", "/w"));
            append(
                &config.path().join("projects/-w/s-a.jsonl"),
                &[
                    r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#,
                    COST_STATE,
                ],
            );
            usage.poll();
            usage.observe(&claude_event("s-b", "/w"));
            if unreadable_second {
                let second = config.path().join("projects/-w/s-b.jsonl");
                append(&second, &[&cost_state(99, "1.0", 1)]);
                #[cfg(unix)]
                if !unreadable(&second) {
                    continue;
                }
            }
            let run = exported(&usage.finish("run5", EndReason::Exited, None));
            assert_eq!(
                run["deck.tokens.unavailable_reason"],
                text("incomplete"),
                "unreadable: {unreadable_second}"
            );
            assert!(!run.contains_key("gen_ai.usage.input_tokens"));
            assert_eq!(run["deck.run.last_turn"], text("unknown"));
            assert_eq!(run["deck.run.termination"], text("unknown"));
        }
    }

    /// A session whose transcript was not read is covered when a later
    /// transcript's running total had started by the time the deck saw it:
    /// that total holds it, and the run's figure is that total.
    #[test]
    fn a_session_not_read_is_covered_by_a_later_total_that_holds_it() {
        let config = tempfile::tempdir().unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        usage.observe(&claude_event("s-gone", "/w"));
        usage.observe(&claude_event("s-later", "/w"));
        append(
            &config.path().join("projects/-w/s-later.jsonl"),
            &[&cost_state(10, "0.0123", now_ms() - 60_000)],
        );
        let run = exported(&usage.finish("run6", EndReason::Exited, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(350));
    }

    /// A second Codex rollout that is missing, or unreadable, makes the
    /// run's tokens and allowance incomplete.
    #[test]
    fn a_codex_rollout_not_read_makes_the_run_incomplete() {
        for unreadable_second in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let day = home.path().join("sessions/2026/10/26");
            let first = day.join("rollout-a.jsonl");
            let second = day.join("rollout-b.jsonl");
            let sample = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}},"rate_limits":{"plan_type":"pro","primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1790500000}}}}"#;
            append(&first, &[sample, sample]);
            let usage = RunUsage::new(&AgentType::Codex, codex_dirs(home.path()), 0);
            usage.observe(&codex_event(&first));
            usage.observe(&codex_event(&second));
            if unreadable_second {
                append(&second, &[CODEX_TOTAL]);
                #[cfg(unix)]
                if !unreadable(&second) {
                    continue;
                }
            }
            let run = exported(&usage.finish("run7", EndReason::Exited, None));
            assert_eq!(
                run["deck.tokens.unavailable_reason"],
                text("incomplete"),
                "unreadable: {unreadable_second}"
            );
            assert_eq!(run["deck.quota.unavailable_reason"], text("incomplete"));
        }
    }

    /// A Codex rollout is read beneath its sessions directory: one whose
    /// ancestor is replaced by a symlink after it was found is not followed
    /// out of it, so the run is incomplete rather than reading the file the
    /// symlink leads to.
    #[cfg(unix)]
    #[test]
    fn a_codex_ancestor_swapped_after_admission_is_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/10/26");
        let rollout = day.join("rollout-swap.jsonl");
        append(&rollout, &[CODEX_TOTAL]);
        let usage = RunUsage::new(&AgentType::Codex, codex_dirs(home.path()), 0);
        usage.observe(&codex_event(&rollout));
        usage.poll();
        let outside = home.path().join("outside");
        append(
            &outside.join("rollout-swap.jsonl"),
            &[
                CODEX_TOTAL,
                &CODEX_TOTAL.replace(r#""input_tokens":1,"#, r#""input_tokens":777,"#),
            ],
        );
        std::fs::rename(&day, home.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &day).unwrap();
        let run = exported(&usage.finish("run8", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
        assert!(!run.contains_key("gen_ai.usage.input_tokens"));
    }

    /// A Claude Code project directory that is a symlink is not read through.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_claude_project_directory_is_not_read() {
        let config = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        append(&outside.path().join("s-9.jsonl"), &[COST_STATE]);
        std::fs::create_dir_all(config.path().join("projects")).unwrap();
        std::os::unix::fs::symlink(outside.path(), config.path().join("projects/-w")).unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        usage.observe(&claude_event("s-9", "/w"));
        let run = exported(&usage.finish("run9", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("not_recorded"));
    }

    /// A Codex rollout outside Codex's own sessions directory is not read.
    #[test]
    fn a_rollout_outside_codex_home_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let rollout = elsewhere.path().join("rollout-x.jsonl");
        append(&rollout, &[CODEX_TOTAL]);
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let usage = RunUsage::new(&AgentType::Codex, codex_dirs(home.path()), 0);
        usage.observe(&codex_event(&rollout));
        let run = exported(&usage.finish("run10", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("not_recorded"));
        assert!(!run.contains_key("gen_ai.usage.input_tokens"));
    }

    const PI_REPLY: &str = r#"{"v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":100,"output":20,"cache_read":50,"cache_write":0,"cost_usd":0.0042,"stop":"stop"}"#;

    /// A Pi run reads the deck's usage file for it, which is removed once
    /// read.
    #[test]
    fn the_usage_file_is_read_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run11.usage.jsonl");
        append(&file, &[]);
        let usage = RunUsage::new(
            &AgentType::Pi,
            usage_file_dirs(dir.path(), "run11.usage.jsonl"),
            0,
        );
        append(&file, &[PI_REPLY]);
        let run = exported(&usage.finish("run11", EndReason::Closed, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(150));
        assert_eq!(run["deck.cost.provenance"], text("estimated"));
        assert_eq!(run["deck.run.termination"], text("completed"));
        assert!(!file.exists());
    }

    /// A Pi that completed one turn, started another and was closed before
    /// that turn's reply ended was interrupted mid-turn, not completed.
    #[test]
    fn a_pi_closed_in_a_later_turn_did_not_complete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run12.usage.jsonl");
        append(
            &file,
            &[r#"{"v":1,"type":"turn_start","agent":"pi"}"#, PI_REPLY],
        );
        let usage = RunUsage::new(
            &AgentType::Pi,
            usage_file_dirs(dir.path(), "run12.usage.jsonl"),
            0,
        );
        usage.poll();
        append(&file, &[r#"{"v":1,"type":"turn_start","agent":"pi"}"#]);
        let run = exported(&usage.finish("run12", EndReason::Closed, None));
        assert_eq!(run["deck.run.last_turn"], text("mid_turn"));
        assert_eq!(run["deck.run.termination"], text("interrupted"));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(150));
    }

    /// A record the agent did not finish writing when the run ended — a last
    /// line with no newline — leaves the run's figure incomplete and its last
    /// turn unknown, for a usage file, a Claude Code transcript and a Codex
    /// rollout alike.
    #[test]
    fn a_record_cut_off_at_the_end_makes_the_run_incomplete() {
        let cut = |line: &str| line[..line.len() / 2].to_string();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run13.usage.jsonl");
        append(&file, &[PI_REPLY]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(cut(PI_REPLY).as_bytes())
            .unwrap();
        let usage = RunUsage::new(
            &AgentType::Pi,
            usage_file_dirs(dir.path(), "run13.usage.jsonl"),
            0,
        );
        let run = exported(&usage.finish("run13", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
        assert_eq!(run["deck.run.last_turn"], text("unknown"));

        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("projects/-w/s-cut.jsonl");
        append(&path, &[COST_STATE]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(cut(COST_STATE).as_bytes())
            .unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        usage.observe(&claude_event("s-cut", "/w"));
        let run = exported(&usage.finish("run14", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));

        let home = tempfile::tempdir().unwrap();
        let rollout = home.path().join("sessions/rollout-cut.jsonl");
        append(&rollout, &[CODEX_TOTAL]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap()
            .write_all(cut(CODEX_TOTAL).as_bytes())
            .unwrap();
        let usage = RunUsage::new(&AgentType::Codex, codex_dirs(home.path()), 0);
        usage.observe(&codex_event(&rollout));
        let run = exported(&usage.finish("run15", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
    }

    /// An arbitrary command has no usage channel, and its exit status
    /// decides how it ended.
    #[test]
    fn a_command_ends_by_its_exit_status() {
        let ended = |code| {
            let usage = RunUsage::new(&AgentType::None, UsageDirs::default(), 0);
            exported(&usage.finish("run16", EndReason::Exited, code))
        };
        let failed = ended(Some(3));
        assert_eq!(failed["deck.run.termination"], text("failed"));
        assert_eq!(failed["deck.run.termination.provenance"], text("measured"));
        assert_eq!(failed["error.type"], text("nonzero_exit"));
        assert_eq!(failed["deck.tokens.unavailable_reason"], text("no_channel"));
        let completed = ended(Some(0));
        assert_eq!(completed["deck.run.termination"], text("completed"));
        assert!(!completed.contains_key("error.type"));
        assert_eq!(ended(None)["deck.run.termination"], text("unknown"));
    }

    /// Sessions past [`MAX_SESSIONS`] are not read, and the run's usage is
    /// incomplete rather than the part that was.
    #[test]
    fn too_many_sessions_make_the_usage_incomplete() {
        let config = tempfile::tempdir().unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        for n in 0..=MAX_SESSIONS {
            usage.observe(&claude_event(&format!("s-{n}"), "/w"));
        }
        append(&config.path().join("projects/-w/s-0.jsonl"), &[COST_STATE]);
        let run = exported(&usage.finish("run17", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
    }

    /// Noting a session reads nothing and does not wait for a poll that is
    /// reading: the readers and the observed sessions are behind separate
    /// locks.
    #[test]
    fn observing_does_not_wait_for_a_poll() {
        let config = tempfile::tempdir().unwrap();
        let usage = RunUsage::new(&AgentType::ClaudeCode, claude_dirs(config.path()), 0);
        let held = lock(&usage.inner.readers);
        usage.observe(&claude_event("s-1", "/w"));
        drop(held);
        assert_eq!(lock(&usage.inner.inbox).claude.len(), 1);
    }
}
