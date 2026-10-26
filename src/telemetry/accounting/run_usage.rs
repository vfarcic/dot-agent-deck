//! PRD #1858: one live run's usage readers, from the run's start to its end.
//!
//! The run tracker holds one [`RunUsage`] per open run. It learns where the
//! run's usage record is from what the deck already knows — the directories
//! an agent keeps its records in ([`UsageDirs`], resolved as the run is
//! prepared), the session id and working directory on Claude Code's events,
//! the rollout path on Codex's — opens a reader once the record is there,
//! polls it on the tracker's poll, and when the run ends drains it and turns
//! what it read into the run's record ([`RunUsage::finish`]).
//!
//! Nothing here logs, and nothing it holds reaches a span but the record's
//! export ([`super::run_attributes`]): a path, a session id or a working
//! directory stays in this struct and is dropped with it.

use std::path::{Path, PathBuf};

use crate::event::{AgentEvent, AgentType};

use super::claude::{ClaudeTranscriptSummary, combine_transcripts};
use super::locate;
use super::usage_file::{UsageAgent, UsageFileReader, UsageFileSummary};
use super::{
    AgentUsage, CodexRolloutSummary, Ident, JsonlReader, LineSummary, RunExport, RunIdentity,
    RunRecord, Unavailable, UsageSource, combine_sessions, command_termination, from_claude,
    from_codex, no_usage_channel, run_attributes, usage_source, usage_unavailable,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageDirs {
    /// Claude Code's configuration directory ([`locate::claude_config_dir`]).
    pub claude_config: Option<PathBuf>,
    /// Codex's home ([`locate::codex_home`]).
    pub codex_home: Option<PathBuf>,
    /// The deck's usage file for the run ([`locate::usage_file`]), which the
    /// deck removes once the run has been read.
    pub usage_file: Option<PathBuf>,
}

/// One session's record: located (and admitted) once it is there.
struct Session<S> {
    reader: Option<JsonlReader<S>>,
    /// Whether the record was already there when the run began.
    preexisting: bool,
}

struct ClaudeSession {
    session_id: String,
    cwd: Option<String>,
    record: Session<ClaudeTranscriptSummary>,
}

struct CodexSession {
    reported: String,
    record: Session<CodexRolloutSummary>,
}

/// See the module docs.
pub struct RunUsage {
    source: UsageSource,
    agent: AgentType,
    dirs: UsageDirs,
    /// When the run began, in nanoseconds since the Unix epoch.
    started: u64,
    claude: Vec<ClaudeSession>,
    codex: Vec<CodexSession>,
    usage_file: Option<UsageFileReader>,
    /// More sessions than [`MAX_SESSIONS`] were seen.
    sessions_lost: bool,
}

impl RunUsage {
    pub fn new(agent: &AgentType, dirs: UsageDirs, started: u64) -> Self {
        let source = usage_source(agent);
        let usage_file = match source {
            UsageSource::UsageFile => dirs.usage_file.clone().map(UsageFileReader::new),
            _ => None,
        };
        RunUsage {
            source,
            agent: agent.clone(),
            dirs,
            started,
            claude: Vec::new(),
            codex: Vec::new(),
            usage_file,
            sessions_lost: false,
        }
    }

    /// An event of the run: a Claude Code event names its session and
    /// working directory, a Codex event its rollout.
    pub fn observe(&mut self, event: &AgentEvent) {
        match self.source {
            UsageSource::ClaudeTranscript if event.agent_type == AgentType::ClaudeCode => {
                let id = event.session_id.as_str();
                if !locate::admissible_session_id(id)
                    || self.claude.iter().any(|s| s.session_id == id)
                {
                    return;
                }
                if self.claude.len() >= MAX_SESSIONS {
                    self.sessions_lost = true;
                    return;
                }
                self.claude.push(ClaudeSession {
                    session_id: id.to_string(),
                    cwd: event.cwd.clone(),
                    record: Session {
                        reader: None,
                        preexisting: false,
                    },
                });
            }
            UsageSource::CodexRollout => {
                let Some(reported) = event.metadata.get(CODEX_TRANSCRIPT_PATH_METADATA_KEY) else {
                    return;
                };
                if self.codex.iter().any(|s| &s.reported == reported) {
                    return;
                }
                if self.codex.len() >= MAX_SESSIONS {
                    self.sessions_lost = true;
                    return;
                }
                self.codex.push(CodexSession {
                    reported: reported.clone(),
                    record: Session {
                        reader: None,
                        preexisting: false,
                    },
                });
            }
            _ => {}
        }
    }

    /// Read what each record gained since the last poll, locating the ones
    /// not found yet where their directory name says. A transcript that was
    /// there before the run began has its total counted from where it was
    /// when the reader first caught up with it.
    pub fn poll(&mut self) {
        self.poll_with(false);
    }

    fn poll_with(&mut self, search: bool) {
        let started = self.started;
        if let Some(config) = self.dirs.claude_config.as_deref() {
            for s in &mut self.claude {
                if s.record.reader.is_none() {
                    let found = if search {
                        locate::claude_transcript(config, s.cwd.as_deref(), &s.session_id)
                    } else {
                        claude_transcript_in_place(config, s.cwd.as_deref(), &s.session_id)
                    };
                    s.record.open(found, started);
                }
                s.record.poll(ClaudeTranscriptSummary::seal_baseline);
            }
        }
        if let Some(home) = self.dirs.codex_home.as_deref() {
            for s in &mut self.codex {
                if s.record.reader.is_none() {
                    let admitted = locate::admit_codex_rollout(&s.reported, home);
                    s.record.open(admitted, started);
                }
                // A rollout is one Codex session's, and a resumed one is a
                // new rollout, so it has no baseline to seal.
                s.record.poll(|_| {});
            }
        }
        if let Some(reader) = &mut self.usage_file {
            let _ = reader.poll();
        }
    }

    /// The run's record, once it has ended as `end` (with `exit_code`, when
    /// the deck has one): every reader drained first, at most
    /// [`MAX_DRAIN_POLLS`] polls each. The deck's usage file is removed.
    pub fn finish(mut self, run_id: &str, end: EndReason, exit_code: Option<i32>) -> RunExport {
        self.poll_with(true);
        for _ in 1..MAX_DRAIN_POLLS {
            if !self.backlogged() {
                break;
            }
            self.poll_with(false);
        }
        let mut usage = self.usage();
        if self.backlogged() || self.sessions_lost {
            usage.models = usage_unavailable(Unavailable::Incomplete);
        }
        if let Some(path) = &self.dirs.usage_file {
            let _ = std::fs::remove_file(path);
        }
        let agent = crate::telemetry::tracker::agent_name(&self.agent);
        let identity = RunIdentity::new(
            Ident::new(run_id).unwrap_or_else(|| Ident::new("unknown").expect("an identifier")),
            Ident::new(agent).expect("an agent name is an identifier"),
        );
        let mut record = RunRecord::new(identity, usage, Some(end));
        if self.agent == AgentType::None {
            record.termination = command_termination(Some(end), exit_code);
        }
        run_attributes(&record)
    }

    fn backlogged(&self) -> bool {
        let backlog = |stats: super::ReaderStats| stats.backlog > 0;
        self.claude
            .iter()
            .filter_map(|s| s.record.reader.as_ref())
            .any(|r| backlog(r.stats()))
            || self
                .codex
                .iter()
                .filter_map(|s| s.record.reader.as_ref())
                .any(|r| backlog(r.stats()))
            || self.usage_file.as_ref().is_some_and(|r| backlog(r.stats()))
    }

    /// The agent's usage from what the readers read. A session whose record
    /// was never found adds nothing; with none found, the usage is not
    /// recorded.
    fn usage(&self) -> AgentUsage {
        match self.source {
            UsageSource::NoChannel => no_usage_channel(),
            UsageSource::ClaudeTranscript => {
                let read: Vec<&ClaudeTranscriptSummary> = self
                    .claude
                    .iter()
                    .filter_map(|s| s.record.reader.as_ref().map(JsonlReader::summary))
                    .collect();
                if read.is_empty() {
                    from_claude(None)
                } else {
                    combine_transcripts(&read)
                }
            }
            UsageSource::CodexRollout => {
                let read: Vec<AgentUsage> = self
                    .codex
                    .iter()
                    .filter_map(|s| s.record.reader.as_ref())
                    .map(|r| from_codex(r.summary()))
                    .collect();
                if read.is_empty() {
                    from_codex(&CodexRolloutSummary::default())
                } else {
                    combine_sessions(read)
                }
            }
            UsageSource::UsageFile => {
                let agent = match self.agent {
                    AgentType::OpenCode => UsageAgent::OpenCode,
                    _ => UsageAgent::Pi,
                };
                match &self.usage_file {
                    Some(reader) => reader.summary().usage(agent),
                    None => UsageFileSummary::default().usage(agent),
                }
            }
        }
    }
}

impl<S: LineSummary> Session<S> {
    /// Open a reader on `path`, once there is one, noting whether the file
    /// was last changed before the run began.
    fn open(&mut self, path: Option<PathBuf>, started: u64) {
        let Some(path) = path else {
            return;
        };
        self.preexisting = modified_before(&path, started);
        self.reader = Some(JsonlReader::new(path));
    }

    /// Poll the reader, if open; `seal` is applied to a preexisting record's
    /// summary the first time the reader has caught up with it.
    fn poll(&mut self, seal: impl FnOnce(&mut S)) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        if reader.poll().is_ok() && self.preexisting && reader.stats().backlog == 0 {
            seal(reader.summary_mut());
            self.preexisting = false;
        }
    }
}

/// [`locate::claude_transcript`] without its search of every project
/// directory, for the poll: only the file the working directory names.
fn claude_transcript_in_place(
    config: &Path,
    cwd: Option<&str>,
    session_id: &str,
) -> Option<PathBuf> {
    if !locate::admissible_session_id(session_id) {
        return None;
    }
    let path = config
        .join("projects")
        .join(locate::claude_project_dir_name(cwd?)?)
        .join(format!("{session_id}.jsonl"));
    std::fs::symlink_metadata(&path)
        .is_ok_and(|m| m.is_file())
        .then_some(path)
}

/// Whether `path` was last modified before `started` (nanoseconds since the
/// Unix epoch). A time that cannot be read counts as not before.
fn modified_before(path: &Path, started: u64) -> bool {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .is_some_and(|modified| modified < started)
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

    const COST_STATE: &str = r#"{"type":"cost-state","totalCostUSD":0.0123,"startTime":1790000000000,"modelUsage":{"claude-haiku-5-5":{"inputTokens":10,"outputTokens":20,"cacheReadInputTokens":300,"cacheCreationInputTokens":40,"costUSD":0.0123}},"hasUnknownModelCost":false}"#;

    /// A Claude Code run reads the transcript of the session its events
    /// name, even one written after the event that named it, and drains it
    /// at the run's end.
    #[test]
    fn a_claude_run_reads_the_transcript_its_events_name() {
        let config = tempfile::tempdir().unwrap();
        let dirs = UsageDirs {
            claude_config: Some(config.path().to_path_buf()),
            ..UsageDirs::default()
        };
        let mut usage = RunUsage::new(&AgentType::ClaudeCode, dirs, 0);
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

    /// A transcript that was there before the run began (a resumed session)
    /// credits the run only with what its total grew by since.
    #[test]
    fn a_resumed_transcript_counts_from_where_it_was() {
        let config = tempfile::tempdir().unwrap();
        let path = config.path().join("projects/-w/s-2.jsonl");
        append(&path, &[COST_STATE]);
        let dirs = UsageDirs {
            claude_config: Some(config.path().to_path_buf()),
            ..UsageDirs::default()
        };
        let started = crate::telemetry::now_unix_nanos() + 1_000_000_000;
        let mut usage = RunUsage::new(&AgentType::ClaudeCode, dirs, started);
        usage.observe(&claude_event("s-2", "/w"));
        usage.poll();
        append(
            &path,
            &[&COST_STATE
                .replace(r#""inputTokens":10"#, r#""inputTokens":15"#)
                .replace(r#""costUSD":0.0123"#, r#""costUSD":0.0200"#)],
        );
        let run = exported(&usage.finish("run2", EndReason::Exited, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(5));
        assert_eq!(run["gen_ai.usage.output_tokens"], int(0));
        assert_eq!(run["deck.cost.microusd"], int(7700));
    }

    /// A Codex rollout outside Codex's own sessions directory is not read.
    #[test]
    fn a_rollout_outside_codex_home_is_not_read() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let rollout = elsewhere.path().join("rollout-x.jsonl");
        append(
            &rollout,
            &[
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}}}}"#,
            ],
        );
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        let dirs = UsageDirs {
            codex_home: Some(home.path().to_path_buf()),
            ..UsageDirs::default()
        };
        let mut usage = RunUsage::new(&AgentType::Codex, dirs, 0);
        let mut event = crate::telemetry::tracker::tests::event("p", EventType::Thinking);
        event.metadata.insert(
            CODEX_TRANSCRIPT_PATH_METADATA_KEY.to_string(),
            rollout.to_string_lossy().into_owned(),
        );
        usage.observe(&event);
        let run = exported(&usage.finish("run3", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("not_recorded"));
        assert!(!run.contains_key("gen_ai.usage.input_tokens"));
    }

    /// A Pi run reads the deck's usage file for it, which is removed once
    /// read.
    #[test]
    fn the_usage_file_is_read_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run4.usage.jsonl");
        let dirs = UsageDirs {
            usage_file: Some(file.clone()),
            ..UsageDirs::default()
        };
        let usage = RunUsage::new(&AgentType::Pi, dirs, 0);
        append(
            &file,
            &[
                r#"{"v":1,"type":"reply","agent":"pi","model":"gpt-5.4-mini","input":100,"output":20,"cache_read":50,"cache_write":0,"cost_usd":0.0042,"stop":"stop"}"#,
            ],
        );
        let run = exported(&usage.finish("run4", EndReason::Closed, None));
        assert_eq!(run["gen_ai.usage.input_tokens"], int(150));
        assert_eq!(run["deck.cost.provenance"], text("estimated"));
        assert_eq!(run["deck.run.termination"], text("completed"));
        assert!(!file.exists());
    }

    /// An arbitrary command has no usage channel, and its exit status
    /// decides how it ended.
    #[test]
    fn a_command_ends_by_its_exit_status() {
        let ended = |code| {
            let usage = RunUsage::new(&AgentType::None, UsageDirs::default(), 0);
            exported(&usage.finish("run5", EndReason::Exited, code))
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
        let dirs = UsageDirs {
            claude_config: Some(config.path().to_path_buf()),
            ..UsageDirs::default()
        };
        let mut usage = RunUsage::new(&AgentType::ClaudeCode, dirs, 0);
        for n in 0..=MAX_SESSIONS {
            usage.observe(&claude_event(&format!("s-{n}"), "/w"));
        }
        append(&config.path().join("projects/-w/s-0.jsonl"), &[COST_STATE]);
        let run = exported(&usage.finish("run6", EndReason::Exited, None));
        assert_eq!(run["deck.tokens.unavailable_reason"], text("incomplete"));
    }
}
