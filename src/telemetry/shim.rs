//! The `PATH` shim that switches on Codex's and Devin's own telemetry at the
//! moment they really start, however they are started.
//!
//! With telemetry on, the daemon writes a small `sh` script per agent —
//! `codex` and `devin` — into a directory of its own ([`install`]) and puts
//! that directory first on the `PATH` of every agent it starts. A pane that
//! runs `codex`, a script that runs it by name (`devbox run codex-big`), or
//! another agent in the pane that runs it, all reach the script, which
//! `exec`s the deck's hidden entrypoint ([`SUBCOMMAND`], [`run_if_invoked`]).
//! That entrypoint:
//!
//! 1. finds the real program: the first executable file of that name on
//!    `PATH` that is not a deck shim script, in the shim directory or any
//!    other (`resolve`) — none is the shell's `command not found` and
//!    status 127;
//! 2. decides from its own environment — the agent's real one, after any
//!    script or launcher — what the agent needs ([`super::launch`]), unless
//!    this program was already shimmed further up its process tree
//!    ([`GUARD_ENV`]), or the real program is not one of the forms the shim
//!    recognises as the agent itself — a native program, npm's Codex
//!    launcher, a Nix store script — and may be a wrapper that changes that
//!    environment before the agent reads it (`launcher_is_certain`), or the
//!    environment holds a value the decision cannot read exactly;
//! 3. replaces itself with the real program, those arguments in front of the
//!    user's own, so signals, the exit status and the terminal are the
//!    program's own.
//!
//! PRD #1790 stage 3: the shim is also how a restored agent is resumed into
//! its conversation, so the daemon installs it whether or not telemetry is
//! on (Unix), and Claude Code goes through it too, unchanged unless a resume
//! is asked for. With telemetry off its directory goes on no ordinary
//! spawn's `PATH`, only on a restore spawn's. A restore spawn carries a
//! one-shot request in its environment ([`RESUME_AGENT_ENV`],
//! [`RESUME_SESSION_ENV`], [`RESUME_TOKEN_ENV`]). When this program is the
//! one it names, the shim removes the request from the program's environment,
//! so nothing the agent starts is resumed into the same conversation. A call
//! that starts no conversation (a version or help query, a managing
//! subcommand) leaves the token for the launch script's later call that does.
//! Otherwise the shim claims the token exclusively (`<token>.claim` in
//! [`RESUME_ACKS_DIR`], held by descriptor) and publishes one outcome: the
//! agent's resume arguments in front of its own as separate argv entries
//! ([`crate::agent_registry::ResumeSpec`]) with `<token>` acknowledging them,
//! or, for an id that is not of the agent's syntax or arguments that choose
//! another conversation or command, `<token>.refused` with the reason and the
//! program unchanged. A claimed token is never claimed again, whatever its
//! outcome. A minted token names its expiry, after which no shim claims it
//! and its files may be retired; the directory's entries are bounded. A
//! request for another program is left for it.
//!
//! Anything that goes wrong in step 2 runs the program unchanged: the shim
//! never stops an agent from starting. The one refusal is a process tree that
//! has passed through the shim [`MAX_HOPS`] times, which would otherwise keep
//! re-entering it. A script that calls the agent by an absolute path,
//! replaces `PATH`, or puts a directory holding the real agent in front of
//! the shim's, does not reach the shim. There is no shim on Windows.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::config::EnvMap;

/// The deck's hidden subcommand the shim scripts run. Never in the help: it
/// is handled before the command line is parsed.
pub const SUBCOMMAND: &str = "__telemetry-shim";

/// The programs the shim stands in front of.
pub const PROGRAMS: [&str; 3] = ["claude", "codex", "devin"];

/// PRD #1790 stage 3: the program a restore spawn asks to resume, by the
/// name the shim stands in front of ([`crate::agent_registry::ResumeSpec`]).
pub const RESUME_AGENT_ENV: &str = "DOT_AGENT_DECK_RESUME_AGENT";
/// The conversation id to resume it into.
pub const RESUME_SESSION_ENV: &str = "DOT_AGENT_DECK_RESUME_SESSION";
/// The request's one-shot token, the name of its acknowledgement.
pub const RESUME_TOKEN_ENV: &str = "DOT_AGENT_DECK_RESUME_TOKEN";
/// The request's variables, removed from the environment of the program the
/// request was for.
pub const RESUME_ENV_KEYS: [&str; 3] = [RESUME_AGENT_ENV, RESUME_SESSION_ENV, RESUME_TOKEN_ENV];

/// The directory beside the shim's, in the deck's telemetry directory, that
/// holds resume acknowledgements: `<token>` when the shim resumed the agent,
/// `<token>.refused` holding the reason when it did not.
pub const RESUME_ACKS_DIR: &str = "resume-acks";
/// The suffix of a refused request's acknowledgement.
pub const REFUSED_SUFFIX: &str = ".refused";
/// A refusal's reason: the agent's own arguments choose another
/// conversation, or a command a resume cannot be combined with.
pub const REFUSED_ARGS_CONFLICT: &str = "resume-args-conflict";
/// A refusal's reason: the id is not of the agent's own syntax.
pub const REFUSED_SESSION_ID_INVALID: &str = "session-id-invalid";
/// A refusal's reason: the program has no resume by id.
pub const REFUSED_AGENT_CANNOT_RESUME: &str = "agent-cannot-resume";

/// The suffix of a token's claim: the one exclusive reservation a resume
/// and a refusal share (audit S2).
pub const CLAIM_SUFFIX: &str = ".claim";

/// The longest resume token the shim accepts.
const MAX_TOKEN_LEN: usize = 64;

/// Audit S3: the most entries the acknowledgements directory holds before
/// the shim claims no new token and the daemon starts no new restore.
pub const MAX_ACK_ENTRIES: usize = 512;

/// The longest a minted token stays usable: an expiry further from now is
/// not claimed.
pub const MAX_TOKEN_LIFETIME_SECS: u64 = 60 * 60;

/// How long after a token's expiry its files are kept before
/// [`retire_expired`] removes them: room for a clock stepped backwards.
pub const ACK_RETIRE_GRACE_SECS: u64 = 60 * 60;

/// The most names one [`retire_expired`] call looks at.
const MAX_RETIRE_LISTED: usize = 4 * MAX_ACK_ENTRIES;

/// The resume acknowledgements' directory for the shim directory `shim_dir`
/// ([`install`]'s answer).
pub fn resume_acks_dir(shim_dir: &Path) -> Option<PathBuf> {
    shim_dir.parent().map(|dir| dir.join(RESUME_ACKS_DIR))
}

/// Whether `token` may name a resume acknowledgement: a plain file name of
/// ASCII letters, digits, `-` and `_`, not too long.
pub fn is_resume_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The shim directory's name inside the deck's telemetry directory.
pub const DIR_NAME: &str = "bin";

/// The deck's run id, in the environment of every agent the deck started a
/// run for, shim or not: what a Devin's per-run configuration is named for, so
/// the daemon removes it when the run ends, and what a tool the agent runs
/// joins its spans to the run by when `OTEL_RESOURCE_ATTRIBUTES` was removed
/// on the way (issue #1893). Blank for an agent the deck started no run for,
/// so one inherited from another deck's agent does not pass through (#1902).
pub const RUN_ID_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_RUN_ID";

/// The programs already shimmed in this process tree, `:`-separated: set on
/// the program the shim runs, so a program that runs itself again by name
/// (a wrapper of the user's called `codex`, say) gets the deck's additions
/// once.
pub const GUARD_ENV: &str = "DOT_AGENT_DECK_TELEMETRY_SHIMMED";

/// How many times one process tree may pass through the shim — the entries
/// in [`GUARD_ENV`] — before the shim refuses to run the program again: far
/// more than agents starting agents need, and a bound on anything that keeps
/// bringing a launch back to the shim.
pub const MAX_HOPS: usize = 16;

/// How much of a candidate program the shim reads to tell whether it is a
/// deck shim script.
const HEAD_LEN: u64 = 16 * 1024;

/// The largest script the shim reads in full to tell whether it may add to
/// it; a larger one gets nothing.
const SCRIPT_LIMIT: u64 = 64 * 1024;

/// Write the shim scripts into `config_dir`'s [`DIR_NAME`], each running the
/// deck at `deck`, and return that directory: owner-only, as are the scripts.
/// Each script is written beside its final name and renamed onto it, so an
/// agent starting meanwhile runs the old one or the new one, never half of
/// one. Refused for a directory `PATH` cannot carry (one with a `:`).
#[cfg(unix)]
pub fn install(config_dir: &Path, deck: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = config_dir.join(DIR_NAME);
    let Some(dir_str) = dir.to_str().filter(|d| !d.contains(':')) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the shim directory's path cannot go on PATH",
        ));
    };
    crate::platform::fsperm::ensure_owner_only_dir(config_dir)?;
    crate::platform::fsperm::ensure_owner_only_dir(&dir)?;
    for name in PROGRAMS {
        let staged = dir.join(format!(".{name}.{}", std::process::id()));
        let _ = std::fs::remove_file(&staged);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staged)?;
        file.write_all(script(deck, dir_str, name).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, dir.join(name))?;
    }
    Ok(dir)
}

/// The shim directory this daemon installed last ([`record_installed`]).
static INSTALLED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// PRD #1790 stage 3 (audit "later slices" 5): remember `dir`, the answer of
/// a successful [`install`], as the directory a restore resumes through.
pub fn record_installed(dir: PathBuf) {
    *INSTALLED.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// The shim directory this daemon installed ([`record_installed`]), `None`
/// when no installation succeeded: a restore then resumes nothing.
pub fn installed_dir() -> Option<PathBuf> {
    INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The shim script for `name`: run the deck at `deck` as the shim in `dir`,
/// with every argument passed on as it came.
fn script(deck: &str, dir: &str, name: &str) -> String {
    let quote = super::launch::shell_quote;
    format!(
        "#!/bin/sh\n\
         # Written by dot-agent-deck: applies the deck's launch additions (telemetry, a restore), then runs the agent.\n\
         exec {} {SUBCOMMAND} {name} {} -- \"$@\"\n",
        quote(deck),
        quote(dir),
    )
}

/// The deck's entrypoint when this process is a shim — its first argument is
/// [`SUBCOMMAND`] — run before anything else reads the command line, which
/// holds the agent's arguments, not the deck's. `None` for any other
/// invocation. Returns only when the program could not be run.
pub fn run_if_invoked() -> Option<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new(SUBCOMMAND)) {
        return None;
    }
    let (Some(name), Some(dir), Some(dashes)) = (args.next(), args.next(), args.next()) else {
        eprintln!("dot-agent-deck: {SUBCOMMAND}: expected <program> <dir> -- <args>");
        return Some(ExitCode::from(2));
    };
    let name = name.to_string_lossy().into_owned();
    if dashes != "--" || !PROGRAMS.contains(&name.as_str()) {
        eprintln!("dot-agent-deck: {SUBCOMMAND}: expected <program> <dir> -- <args>");
        return Some(ExitCode::from(2));
    }
    Some(run(&name, Path::new(&dir), args.collect()))
}

/// What the shim runs: the real program, its arguments, and the variables
/// removed from its environment.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    program: PathBuf,
    args: Vec<OsString>,
    remove_env: Vec<OsString>,
}

/// A restore spawn's one-shot resume request, as the shim's environment
/// carries it: `None` fields are unset.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ResumeRequest {
    agent: Option<String>,
    session: Option<String>,
    token: Option<String>,
}

impl ResumeRequest {
    fn from_env(env: &EnvMap) -> Self {
        let get = |key: &str| env.get(key).filter(|v| !v.is_empty()).cloned();
        ResumeRequest {
            agent: get(RESUME_AGENT_ENV),
            session: get(RESUME_SESSION_ENV),
            token: get(RESUME_TOKEN_ENV),
        }
    }
}

#[cfg(unix)]
fn run(name: &str, dir: &Path, args: Vec<OsString>) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let path = std::env::var_os("PATH");
    let guard = std::env::var_os(GUARD_ENV);
    let env = super::config::process_env();
    // The request is read whether or not the telemetry decision can read the
    // whole environment exactly: its own values are plain ASCII.
    let resume = ResumeRequest::from_env(&env);
    let plan = match plan_with(
        name,
        dir,
        args,
        path.as_deref(),
        guard.as_deref(),
        exact_env().then_some(&env),
        &resume,
        std::process::id(),
    ) {
        Ok(plan) => plan,
        Err(Refused::NotFound) => {
            eprintln!("{name}: command not found");
            return ExitCode::from(127);
        }
        Err(Refused::TooManyHops) => {
            eprintln!(
                "{name}: not started: dot-agent-deck's telemetry shim was reached {MAX_HOPS} times in this process tree, so something keeps running it again instead of {name}; check the {name} entries on PATH"
            );
            return ExitCode::from(126);
        }
    };
    let mut command = std::process::Command::new(&plan.program);
    command
        .arg0(name)
        .args(&plan.args)
        .env(GUARD_ENV, guarded(guard.as_deref(), name));
    for key in &plan.remove_env {
        command.env_remove(key);
    }
    let e = command.exec();
    eprintln!("{name}: {e}");
    ExitCode::from(if e.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    })
}

#[cfg(not(unix))]
fn run(name: &str, _dir: &Path, _args: Vec<OsString>) -> ExitCode {
    eprintln!("{name}: the telemetry shim runs on Unix only");
    ExitCode::from(127)
}

/// Whether the decision can read every variable it uses exactly as the agent
/// will: [`super::config::process_env`] leaves out a value that is not UTF-8,
/// which for a configuration home or an `OTEL_*` variable would have the
/// decision read another configuration than the agent's.
#[cfg(unix)]
fn exact_env() -> bool {
    std::env::vars_os().all(|(key, value)| {
        value.to_str().is_some()
            || !key.to_str().is_some_and(|k| {
                k.starts_with("OTEL_") || super::launch::CONFIG_HOME_ENV_KEYS.contains(&k)
            })
    })
}

/// Why the shim runs no program.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// No real program of that name on `PATH`.
    NotFound,
    /// The process tree has passed through the shim [`MAX_HOPS`] times.
    TooManyHops,
}

/// The shim's plan for `name` called with `args`, with `PATH` = `path`,
/// [`GUARD_ENV`] = `guard` and the environment `env` — `None` when it cannot
/// be read exactly, which adds nothing; the program will have process id
/// `pid`. A resume request is read from `env` ([`plan_with`]).
#[cfg_attr(not(test), allow(dead_code))]
fn plan(
    name: &str,
    dir: &Path,
    args: Vec<OsString>,
    path: Option<&OsStr>,
    guard: Option<&OsStr>,
    env: Option<&EnvMap>,
    pid: u32,
) -> Result<Plan, Refused> {
    let resume = env.map(ResumeRequest::from_env).unwrap_or_default();
    plan_with(name, dir, args, path, guard, env, &resume, pid)
}

/// [`plan`], with the resume request `resume` read separately from the
/// environment the telemetry decision reads.
#[allow(clippy::too_many_arguments)]
fn plan_with(
    name: &str,
    dir: &Path,
    args: Vec<OsString>,
    path: Option<&OsStr>,
    guard: Option<&OsStr>,
    env: Option<&EnvMap>,
    resume: &ResumeRequest,
    pid: u32,
) -> Result<Plan, Refused> {
    let shimmed = guard.map(|g| g.to_string_lossy().into_owned());
    let shimmed: Vec<&str> = shimmed
        .as_deref()
        .map(|g| g.split(':').filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    if shimmed.len() >= MAX_HOPS {
        return Err(Refused::TooManyHops);
    }
    let program = resolve(name, path, dir).ok_or(Refused::NotFound)?;
    // A request for this program is consumed here, resumed or refused; one
    // for another program is left in the environment for it.
    let (resumed, remove_env) = if resume.agent.as_deref() == Some(name) {
        (
            resume_args(name, dir, &args, resume),
            RESUME_ENV_KEYS.iter().map(OsString::from).collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let added = match env {
        Some(env) if !shimmed.contains(&name) && launcher_is_certain(name, &program) => {
            additions(name, dir, &args, env, pid)
        }
        _ => Vec::new(),
    };
    // Issue #2002: the wrapper's embedded-mode pair stays first, in front of
    // these additions as well as the user's own arguments.
    let mut args = args;
    let rest = args.split_off(
        if name == "codex" && crate::wrap::opens_with_codex_embedded_args(&args) {
            crate::wrap::CODEX_EMBEDDED_ARGS.len()
        } else {
            0
        },
    );
    // PRD #1790 stage 3: the resume arguments go first of all, in front of
    // the embedded-mode pair too: Codex's `resume` is a subcommand.
    Ok(Plan {
        program,
        args: resumed
            .into_iter()
            .chain(args)
            .chain(added.into_iter().map(OsString::from))
            .chain(rest)
            .collect(),
        remove_env,
    })
}

/// The arguments that resume `name` as `request` asks, or none. Resumes only
/// for a usable token ([`token_usable`]), only once per token, and only for a
/// call that starts a conversation: a version or help query, or a managing
/// subcommand ([`crate::agent_registry::ResumeSpec::inspects`], review R3),
/// claims nothing and leaves the request for the launch script's later call
/// that does. Otherwise the token is claimed exclusively ([`settle`]) before
/// anything else happens to it, whatever the outcome: an id that is not of
/// the agent's syntax, or the agent's own `args` choosing another
/// conversation or command, publishes the refused acknowledgement and gives
/// none.
#[cfg(unix)]
fn resume_args(
    name: &str,
    dir: &Path,
    args: &[OsString],
    request: &ResumeRequest,
) -> Vec<OsString> {
    let Some(token) = request.token.as_deref().filter(|t| is_resume_token(t)) else {
        return Vec::new();
    };
    let spec = crate::agent_registry::ALL
        .iter()
        .filter_map(|spec| spec.session_args)
        .find(|resume| resume.program == name);
    if spec.is_some_and(|spec| spec.inspects(args)) {
        return Vec::new();
    }
    if !token_usable(token, now_secs()) {
        return Vec::new();
    }
    let Ok(acks) = open_acks(dir, true) else {
        return Vec::new();
    };
    settle(&acks, token, name, spec, request.session.as_deref(), args)
}

#[cfg(not(unix))]
fn resume_args(
    _name: &str,
    _dir: &Path,
    _args: &[OsString],
    _request: &ResumeRequest,
) -> Vec<OsString> {
    Vec::new()
}

/// Audit S2: claim `token` in the held acknowledgements directory `acks`,
/// then publish its one outcome. The claim is `<token>.claim`, created
/// exclusively ([`CLAIM_SUFFIX`]): the one reservation a refusal and a resume
/// share, so a token refused once is consumed as a resumed one is, and of two
/// calls racing on one token only one decides. The outcome is written beside
/// it and linked into place complete ([`publish`]): `<token>` for a resume,
/// `<token>.refused` for a refusal. A claim with no outcome is a call that
/// failed after claiming ([`AckState::Pending`]), which gives no arguments.
/// The admission bound ([`MAX_ACK_ENTRIES`], audit S3) is checked first: a
/// full directory claims nothing.
#[cfg(unix)]
fn settle(
    acks: &crate::platform::trusted_dir::TrustedDir,
    token: &str,
    name: &str,
    spec: Option<&crate::agent_registry::ResumeSpec>,
    session: Option<&str>,
    args: &[OsString],
) -> Vec<OsString> {
    if count_entries(acks, MAX_ACK_ENTRIES) >= MAX_ACK_ENTRIES {
        return Vec::new();
    }
    if acks
        .create_new_owner_only(&format!("{token}{CLAIM_SUFFIX}"))
        .is_err()
    {
        return Vec::new();
    }
    let session = session.unwrap_or_default();
    let refusal = match spec {
        None => Some(REFUSED_AGENT_CANNOT_RESUME),
        Some(spec) if !spec.valid_session_id(session) => Some(REFUSED_SESSION_ID_INVALID),
        Some(spec) if spec.conflicts(args) => Some(REFUSED_ARGS_CONFLICT),
        Some(_) => None,
    };
    if let Some(reason) = refusal {
        let _ = publish(acks, token, &format!("{token}{REFUSED_SUFFIX}"), reason);
        return Vec::new();
    }
    match (spec, publish(acks, token, token, name)) {
        (Some(spec), Ok(())) => spec.args(session),
        _ => Vec::new(),
    }
}

/// Write `body` and a newline to a staging file beside `name` in `acks`,
/// owner-only and synced, then link it to `name`, which fails when `name`
/// exists: a reader sees no file or the whole one, never part of one.
#[cfg(unix)]
fn publish(
    acks: &crate::platform::trusted_dir::TrustedDir,
    token: &str,
    name: &str,
    body: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let staged = format!(".{token}.tmp");
    #[cfg(test)]
    if test_seams::fail_publish() {
        return Err(std::io::Error::other("publish failure (test seam)"));
    }
    let mut file = acks.create_new_owner_only(&staged)?;
    let linked = file
        .write_all(format!("{body}\n").as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| acks.link(&staged, name));
    drop(file);
    let _ = acks.remove_file(&staged);
    linked?;
    let _ = acks.sync();
    Ok(())
}

/// How many entries `acks` holds, counting at most `limit` (and `limit` when
/// it cannot be listed, so a directory that cannot be read admits nothing).
#[cfg(unix)]
fn count_entries(acks: &crate::platform::trusted_dir::TrustedDir, limit: usize) -> usize {
    let Ok(mut stream) = acks.stream() else {
        return limit;
    };
    let mut count = 0;
    while count < limit {
        match stream.next_name() {
            Some(Ok(_)) => count += 1,
            Some(Err(_)) => return limit,
            None => break,
        }
    }
    count
}

/// The resume acknowledgements' directory of the shim directory `shim_dir`,
/// held (audit S1): the telemetry directory opened as the anchor (owned by
/// this account), then [`RESUME_ACKS_DIR`] below it, created owner-only when
/// `create`, opened without following a link and verified owner-only. Every
/// later operation is relative to the held descriptor, so a directory renamed
/// or replaced by a link after this cannot redirect one.
#[cfg(unix)]
pub(crate) fn open_acks(
    shim_dir: &Path,
    create: bool,
) -> std::io::Result<crate::platform::trusted_dir::TrustedDir> {
    let parent = shim_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the shim directory has no parent",
        )
    })?;
    crate::platform::trusted_dir::TrustedDir::anchor(parent)?.child(RESUME_ACKS_DIR, create)
}

/// Where a restore's token stands, as the daemon reads it ([`ack_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckState {
    /// Nothing claimed it yet.
    Unclaimed,
    /// Claimed, with no outcome published: a call is deciding, or failed
    /// after claiming. Never a resume.
    Pending,
    /// The shim gave the agent its resume arguments.
    Resumed,
    /// The shim refused the request and ran the agent unchanged.
    Refused,
}

/// Where `token` stands in the held acknowledgements directory `acks`: each
/// name looked at without following a link, and only a regular file counts.
#[cfg(unix)]
pub(crate) fn ack_state(acks: &crate::platform::trusted_dir::TrustedDir, token: &str) -> AckState {
    use crate::platform::trusted_dir::EntryKind;
    let is_file = |name: &str| {
        acks.entry(name)
            .is_ok_and(|entry| entry.kind == EntryKind::File)
    };
    if is_file(&format!("{token}{REFUSED_SUFFIX}")) {
        AckState::Refused
    } else if is_file(token) {
        AckState::Resumed
    } else if is_file(&format!("{token}{CLAIM_SUFFIX}")) {
        AckState::Pending
    } else {
        AckState::Unclaimed
    }
}

/// What the shim did with `token`, for the journal's record of a restore
/// that did not take ([`crate::work_journal::ShimOutcome`]): [`ack_state`],
/// with a refusal's reason read from its acknowledgement (a regular file, at
/// most a line's worth, never through a link).
#[cfg(unix)]
pub(crate) fn ack_outcome(
    acks: &crate::platform::trusted_dir::TrustedDir,
    token: &str,
) -> crate::work_journal::ShimOutcome {
    use crate::work_journal::ShimOutcome;
    use std::io::Read;
    match ack_state(acks, token) {
        AckState::Unclaimed => ShimOutcome::NotReached,
        AckState::Pending => ShimOutcome::Pending,
        AckState::Resumed => ShimOutcome::Resumed,
        AckState::Refused => {
            let mut body = String::new();
            let read = acks
                .open_regular(format!("{token}{REFUSED_SUFFIX}"))
                .and_then(|file| file.take(128).read_to_string(&mut body));
            match read.map(|_| body.trim()) {
                Ok(REFUSED_ARGS_CONFLICT) => ShimOutcome::ResumeArgsConflict,
                Ok(REFUSED_SESSION_ID_INVALID) => ShimOutcome::SessionIdInvalid,
                Ok(REFUSED_AGENT_CANNOT_RESUME) => ShimOutcome::AgentCannotResume,
                _ => ShimOutcome::Refused,
            }
        }
    }
}

/// Audit S3: a fresh token, unpredictable (16 bytes of OS randomness) and
/// bound to a lifetime: `<expiry>-<32 hex>`, the expiry in seconds since the
/// Unix epoch, `lifetime` from `now` and at most
/// [`MAX_TOKEN_LIFETIME_SECS`]. `None` without OS randomness.
pub fn mint_token(now: u64, lifetime: u64) -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let expiry = now.saturating_add(lifetime.min(MAX_TOKEN_LIFETIME_SECS));
    Some(format!("{expiry}-{hex}"))
}

/// The expiry a token names ([`mint_token`]), or `None` for a token of
/// another form.
pub fn token_expiry(token: &str) -> Option<u64> {
    let (expiry, rest) = token.split_once('-')?;
    (!rest.is_empty() && !expiry.is_empty() && expiry.len() <= 20)
        .then(|| expiry.parse().ok())
        .flatten()
}

/// Whether the shim may claim `token` at `now`: a token that names an expiry
/// only until it, and only when that expiry is within
/// [`MAX_TOKEN_LIFETIME_SECS`] of `now`. A token of another form has no time
/// bound; the admission bound ([`MAX_ACK_ENTRIES`]) is what limits those.
fn token_usable(token: &str, now: u64) -> bool {
    token_expiry(token)
        .is_none_or(|expiry| expiry >= now && expiry <= now.saturating_add(MAX_TOKEN_LIFETIME_SECS))
}

/// Seconds since the Unix epoch, now.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Audit S3: remove from `acks` every file of a token whose expiry passed
/// more than [`ACK_RETIRE_GRACE_SECS`] before `now`: its claim, its outcome
/// and a staging file left behind. Safe across daemons and without weakening
/// replay protection: a token past its expiry is claimed by no shim
/// ([`token_usable`]), so its files are no longer what stops a replay.
/// Files of a token of another form are never removed. Looks at most
/// [`MAX_RETIRE_LISTED`] names per call. Answers how many entries remain.
#[cfg(unix)]
pub(crate) fn retire_expired(acks: &crate::platform::trusted_dir::TrustedDir, now: u64) -> usize {
    let Ok(mut stream) = acks.stream() else {
        return MAX_ACK_ENTRIES;
    };
    let mut names = Vec::new();
    while names.len() < MAX_RETIRE_LISTED {
        match stream.next_name() {
            Some(Ok(name)) => names.push(name),
            Some(Err(_)) | None => break,
        }
    }
    drop(stream);
    let mut remaining = 0;
    for name in names {
        let expired = name.to_str().is_some_and(|name| {
            let token = name.trim_start_matches('.');
            let token = token.split_once('.').map_or(token, |(token, _)| token);
            token_expiry(token)
                .is_some_and(|expiry| expiry.saturating_add(ACK_RETIRE_GRACE_SECS) < now)
        });
        if !(expired && acks.remove_file(&name).is_ok()) {
            remaining += 1;
        }
    }
    remaining
}

#[cfg(test)]
pub(crate) mod test_seams {
    use std::cell::Cell;

    thread_local! {
        static FAIL_PUBLISH: Cell<bool> = const { Cell::new(false) };
    }

    /// Make this thread's next [`super::publish`] calls fail, until reset.
    pub(crate) fn set_fail_publish(fail: bool) {
        FAIL_PUBLISH.with(|f| f.set(fail));
    }

    pub(super) fn fail_publish() -> bool {
        FAIL_PUBLISH.with(Cell::get)
    }
}

/// The arguments the deck puts in front of `name`'s own `args`, or none —
/// whatever goes wrong deciding them, a panic included, is "none".
fn additions(name: &str, dir: &Path, args: &[OsString], env: &EnvMap, pid: u32) -> Vec<String> {
    let decide = || match name {
        "codex" => super::launch::codex_args(args, env),
        "devin" => {
            let (Some(run_id), Some(config_dir)) =
                (super::config::non_empty(env, RUN_ID_ENV), dir.parent())
            else {
                return Vec::new();
            };
            let run = super::launch::DevinRun {
                run_id,
                config_dir,
                pid,
            };
            super::launch::devin_args(args, env, &run).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let quiet = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(decide));
    std::panic::set_hook(quiet);
    decided.unwrap_or_default()
}

/// `guard` with `name` added.
fn guarded(guard: Option<&OsStr>, name: &str) -> OsString {
    match guard.filter(|g| !g.is_empty()) {
        Some(g) => {
            let mut out = g.to_os_string();
            out.push(":");
            out.push(name);
            out
        }
        None => OsString::from(name),
    }
}

/// The program `name` as a shell would find it on `path`, skipping the
/// deck's shims: a file that, symlinks resolved, is in the shim directory
/// `dir` — so an entry that is the directory, or a symlink to it, or a link
/// to one of its scripts, is passed over — and any file that reads as a shim
/// script ([`is_shim_script`]), so another deck's shim directory, or a copy
/// or a hard link of a script, is passed over too. An empty entry is the
/// current directory, as for the shell.
fn resolve(name: &str, path: Option<&OsStr>, dir: &Path) -> Option<PathBuf> {
    let shim = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    std::env::split_paths(path?)
        .map(|entry| {
            if entry.as_os_str().is_empty() {
                PathBuf::from(".").join(name)
            } else {
                entry.join(name)
            }
        })
        .find(|candidate| {
            std::fs::canonicalize(candidate).is_ok_and(|c| c.parent() != Some(shim.as_path()))
                && is_executable_file(candidate)
                && !is_shim_file(candidate)
        })
}

/// Whether the file at `path` reads as a shim script ([`is_shim_script`]),
/// whichever deck and directory it names. A test that resolves the real agent
/// from a `PATH` a deck put its shim directory on asks this too, or it runs
/// the shim as the agent.
pub fn is_shim_file(path: &Path) -> bool {
    head(path).is_some_and(|h| is_shim_script(&h))
}

/// The first [`HEAD_LEN`] bytes of the file at `path`, or `None` when it
/// cannot be read.
fn head(path: &Path) -> Option<Vec<u8>> {
    read_up_to(path, HEAD_LEN)
}

/// The file at `path` up to `limit` bytes and one more, so a caller can tell
/// a file longer than `limit`; `None` when it cannot be read.
fn read_up_to(path: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// Whether `head`, the start of a file, is a shim script as [`script`] writes
/// them, whichever deck and directory it names.
fn is_shim_script(head: &[u8]) -> bool {
    let needle = format!(" {SUBCOMMAND} ");
    head.starts_with(b"#!")
        && head.split(|b| *b == b'\n').any(|line| {
            line.starts_with(b"exec ") && line.windows(needle.len()).any(|w| w == needle.as_bytes())
        })
}

/// Whether `program`, the real `name` the shim is about to run, is one of the
/// forms the shim adds to. Anything else may be a wrapper of the user's that
/// changes the agent's configuration home, or anything else the decision
/// reads, before running the agent, so the decision would have read the wrong
/// configuration; the shim declines it rather than guess what it does. The
/// forms, by the file with symlinks resolved:
///
/// - a native executable ([`is_native`]);
/// - for Codex, npm's launcher: `bin/codex.js` in an `@openai/codex` package
///   ([`is_codex_npm_launcher`]);
/// - a script in the Nix store whose text names neither a configuration home
///   nor an `OTEL_` variable ([`is_nix_store_path`],
///   [`names_no_decision_variable`]).
///
/// A file it cannot read is none of them.
fn launcher_is_certain(name: &str, program: &Path) -> bool {
    let Ok(canonical) = std::fs::canonicalize(program) else {
        return false;
    };
    read_up_to(&canonical, SCRIPT_LIMIT).is_some_and(|text| is_recognised(name, &canonical, &text))
}

/// [`launcher_is_certain`] for the file at `canonical`, symlinks resolved,
/// whose first [`SCRIPT_LIMIT`] bytes and one more are `text`.
fn is_recognised(name: &str, canonical: &Path, text: &[u8]) -> bool {
    is_native(text)
        || (name == "codex" && is_codex_npm_launcher(canonical))
        || (is_nix_store_path(canonical)
            && text.starts_with(b"#!")
            && text.len() as u64 <= SCRIPT_LIMIT
            && names_no_decision_variable(text))
}

/// Whether `head`, the start of a file, is an ELF or Mach-O executable.
fn is_native(head: &[u8]) -> bool {
    const MAGICS: [[u8; 4]; 6] = [
        [0x7f, b'E', b'L', b'F'],
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
    ];
    MAGICS.iter().any(|m| head.starts_with(m))
}

/// Whether `canonical`, a program's path with symlinks resolved, is the
/// launcher npm installs for Codex: the `bin` entry of the `@openai/codex`
/// package, `bin/codex.js`, wherever the package's `node_modules` is.
fn is_codex_npm_launcher(canonical: &Path) -> bool {
    canonical.ends_with("node_modules/@openai/codex/bin/codex.js")
}

/// Whether `canonical`, a program's path with symlinks resolved, is in the Nix
/// store, which Nix keeps read-only: a script there is one a Nix build wrote,
/// not one edited in place.
fn is_nix_store_path(canonical: &Path) -> bool {
    canonical.starts_with("/nix/store/")
}

/// Whether `text`, a script, names neither `HOME` (which `CODEX_HOME` and
/// `XDG_CONFIG_HOME` contain) nor an `OTEL_` variable, the configuration the
/// decision reads. A check of the text, not of what the script does: a name
/// the script computes or splits, and a script that runs another one that
/// sets them, are not caught.
fn names_no_decision_variable(text: &[u8]) -> bool {
    !["HOME", "OTEL_"]
        .iter()
        .any(|v| text.windows(v.len()).any(|w| w == v.as_bytes()))
}

/// Whether `path` is, symlinks followed, a regular file this process may
/// execute.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a NUL-terminated path that outlives the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use spec::spec;
    use std::os::unix::fs::PermissionsExt;

    const RUN: &str = "0123456789abcdef0123456789abcdef";

    /// A stand-in for a real agent: a native program, as far as the start of
    /// its file goes — the shim reads it and never runs it here.
    fn executable(path: &Path) {
        crate::test_isolation::write_script(path, b"\x7fELF\x02\x01\x01\x00").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    const RESUME_ID: &str = "01234567-89ab-4cde-8fab-0123456789ab";
    const RESUME_ENV_KEYS: [&str; 3] = [
        "DOT_AGENT_DECK_RESUME_AGENT",
        "DOT_AGENT_DECK_RESUME_SESSION",
        "DOT_AGENT_DECK_RESUME_TOKEN",
    ];

    fn resume_env(program: &str, id: &str, token: &str) -> EnvMap {
        env(&[
            (RESUME_ENV_KEYS[0], program),
            (RESUME_ENV_KEYS[1], id),
            (RESUME_ENV_KEYS[2], token),
        ])
    }

    fn assert_resume_env_removed(plan: &Plan) {
        for key in RESUME_ENV_KEYS {
            assert!(
                plan.remove_env.contains(&OsString::from(key)),
                "child retains {key}: {plan:?}"
            );
        }
    }

    /// Scenario: Launch Claude or Codex through the shim with a one-shot resume request, including a shell-script launcher and Codex's embedded arguments. The child receives separate resume arguments, loses the request environment and gets a private exclusive acknowledgement; unrelated launches pass through.
    #[spec("journal/restore/004")]
    #[test]
    fn journal_restore_004_the_shim_resumes_once_before_user_and_embedded_args() {
        for (program, prefix) in [("claude", "--resume"), ("codex", "resume")] {
            let f = fixture();
            // A wrapper is deliberately not a telemetry-certain launcher.
            crate::test_isolation::write_script(f.real.join(program), b"#!/bin/sh\nexit 0\n")
                .unwrap();
            let request = resume_env(program, RESUME_ID, "restore-token");
            let mut input = if program == "codex" {
                crate::wrap::CODEX_EMBEDDED_ARGS.to_vec()
            } else {
                Vec::new()
            };
            input.extend(["--model", "mini"]);
            let planned = f.plan(program, &input, &request).unwrap();
            let mut expected = os(&[prefix, RESUME_ID]);
            expected.extend(os(&input));
            assert_eq!(planned.program, f.real.join(program));
            assert_eq!(planned.args, expected, "{program}");
            assert_resume_env_removed(&planned);
            let acks = f.telemetry.join("resume-acks");
            let ack = acks.join("restore-token");
            assert!(ack.is_file(), "shim did not acknowledge {program}");
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&acks), 0o700);
            assert_eq!(mode(&ack), 0o600);
            assert!(!acks.join("restore-token.refused").exists());

            // O_EXCL: a second plan cannot overwrite the first acknowledgement.
            std::fs::write(&ack, "first-owner").unwrap();
            let repeated = f.plan(program, &input, &request).unwrap();
            assert_eq!(std::fs::read_to_string(&ack).unwrap(), "first-owner");
            assert_eq!(
                repeated.args,
                os(&input),
                "a consumed token must not inject again"
            );
            assert_resume_env_removed(&repeated);
        }
        for program in ["claude", "codex"] {
            let f = fixture();
            executable(&f.real.join(program));
            let plain = f.plan(program, &["--model", "mini"], &env(&[])).unwrap();
            assert_eq!(plain.args, os(&["--model", "mini"]));
            assert!(!f.telemetry.join("resume-acks").exists());
            let other = if program == "claude" {
                "codex"
            } else {
                "claude"
            };
            let unmatched = f
                .plan(
                    program,
                    &["--model", "mini"],
                    &resume_env(other, RESUME_ID, "not-mine"),
                )
                .unwrap();
            assert_eq!(unmatched.args, os(&["--model", "mini"]));
            assert!(
                unmatched.remove_env.is_empty(),
                "a launcher for another program must preserve the request"
            );
            assert!(!f.telemetry.join("resume-acks").exists());
        }
        let f = fixture();
        executable(&f.real.join("claude"));
        let plain = f
            .plan("claude", &["--model", "haiku"], &f.env(&[]))
            .unwrap();
        assert_eq!(
            plain.args,
            os(&["--model", "haiku"]),
            "ordinary Claude gains no telemetry args"
        );
    }

    /// Scenario: Ask the shim to resume with an invalid id or launch options that select another conversation or command. It starts the original command unchanged, removes the one-shot environment and writes a refusal with the reason instead of a success acknowledgement.
    #[spec("journal/restore/005")]
    #[test]
    fn journal_restore_005_conflicts_and_bad_ids_write_refused_acks() {
        for (program, args, id, reason) in [
            (
                "claude",
                vec!["--continue"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "claude",
                vec!["--resume", RESUME_ID],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "claude",
                vec!["--resume=some-name"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "claude",
                vec!["--session-id", RESUME_ID],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "claude",
                vec!["--fork-session"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "claude",
                vec!["-p", "hello"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "codex",
                vec!["exec", "hello"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "codex",
                vec!["resume", RESUME_ID],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "codex",
                vec!["hello world"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            (
                "codex",
                vec!["--model", "mini", "hello"],
                RESUME_ID,
                "resume-args-conflict",
            ),
            ("claude", vec![], "--continue", "session-id-invalid"),
            (
                "codex",
                vec![],
                "01234567-89AB-4CDE-8FAB-0123456789AB",
                "session-id-invalid",
            ),
            ("claude", vec![], "$(touch sentinel)", "session-id-invalid"),
        ] {
            let f = fixture();
            executable(&f.real.join(program));
            let planned = f
                .plan(program, &args, &resume_env(program, id, "refused-token"))
                .unwrap();
            assert_eq!(planned.args, os(&args), "{program} {args:?} {id}");
            assert_resume_env_removed(&planned);
            let acks = f.telemetry.join("resume-acks");
            assert!(!acks.join("refused-token").exists());
            let refusal = acks.join("refused-token.refused");
            assert_eq!(std::fs::read_to_string(&refusal).unwrap().trim(), reason);
            assert_eq!(
                std::fs::metadata(&refusal).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    /// PRD #1790 stage 3, lane-2 `journal/restore/017`'s failed run: in a
    /// restored Codex pane the deck's wrapper runs `codex app-server` (its
    /// hooks trust probe) before the agent itself. That call starts no
    /// conversation, so it must leave the one-shot request unclaimed: before
    /// this, the shim refused it there (`app-server` read as a prompt),
    /// consuming the token, and the real launch then started a new
    /// conversation that the daemon stopped as `not-resumed`.
    #[test]
    fn a_codex_app_server_call_leaves_the_resume_for_the_launch() {
        let f = fixture();
        executable(&f.real.join("codex"));
        let id = "01a206e2-2b6a-7412-9069-33b41fd7962e";
        let request = resume_env("codex", id, "restore-token");
        let acks = f.telemetry.join("resume-acks");
        let probe = f.plan("codex", &["app-server"], &request).unwrap();
        assert_eq!(probe.args, os(&["app-server"]));
        assert!(
            !acks.join("restore-token.refused").exists()
                && !acks.join("restore-token.claim").exists(),
            "the probe spent the resume request"
        );
        let launch = [
            "-c",
            "shell_environment_policy.set={}",
            "--model",
            "gpt-5.6-luna",
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "never",
            "-c",
            "model_reasoning_effort=\"low\"",
        ];
        let planned = f.plan("codex", &launch, &request).unwrap();
        let mut expected = os(&["resume", id]);
        expected.extend(os(&launch));
        assert_eq!(planned.args, expected);
        assert!(acks.join("restore-token").is_file());
        assert!(!acks.join("restore-token.refused").exists());
    }

    /// PRD #1790 stage 3 (audit S2): a refusal consumes the token as a resume
    /// does, so the same token with arguments that would resume gets nothing,
    /// and the daemon reads it refused.
    #[test]
    fn a_refused_token_is_consumed_and_a_valid_replay_gets_nothing() {
        let f = fixture();
        executable(&f.real.join("claude"));
        let request = resume_env("claude", RESUME_ID, "replayed-token");
        let refused = f.plan("claude", &["--continue"], &request).unwrap();
        assert_eq!(refused.args, os(&["--continue"]));
        let replay = f.plan("claude", &["--model", "haiku"], &request).unwrap();
        assert_eq!(replay.args, os(&["--model", "haiku"]), "replayed");
        let acks = f.telemetry.join(RESUME_ACKS_DIR);
        assert!(!acks.join("replayed-token").exists());
        let held = open_acks(&f.shim, false).unwrap();
        assert_eq!(ack_state(&held, "replayed-token"), AckState::Refused);
        assert_eq!(ack_state(&held, "never-used"), AckState::Unclaimed);
        use crate::work_journal::ShimOutcome;
        assert_eq!(
            ack_outcome(&held, "replayed-token"),
            ShimOutcome::ResumeArgsConflict
        );
        assert_eq!(ack_outcome(&held, "never-used"), ShimOutcome::NotReached);
        let bad_id = resume_env("claude", "not-a-uuid", "bad-id-token");
        f.plan("claude", &[], &bad_id).unwrap();
        assert_eq!(
            ack_outcome(&held, "bad-id-token"),
            ShimOutcome::SessionIdInvalid
        );
        let resumed = resume_env("claude", RESUME_ID, "resumed-token");
        f.plan("claude", &[], &resumed).unwrap();
        assert_eq!(ack_outcome(&held, "resumed-token"), ShimOutcome::Resumed);
        std::fs::write(acks.join("odd-token.refused"), "something else\n").unwrap();
        assert_eq!(ack_outcome(&held, "odd-token"), ShimOutcome::Refused);
    }

    /// PRD #1790 stage 3 (audit S2): of calls racing on one token with
    /// conflicting and resuming arguments, exactly one claims it and exactly
    /// one outcome is published.
    #[test]
    fn concurrent_calls_on_one_token_publish_one_outcome() {
        for round in 0..8 {
            let f = std::sync::Arc::new(fixture());
            executable(&f.real.join("claude"));
            let token = format!("race-{round}");
            let handles: Vec<_> = (0..6)
                .map(|n| {
                    let f = std::sync::Arc::clone(&f);
                    let token = token.clone();
                    std::thread::spawn(move || {
                        let args: &[&str] = if n % 2 == 0 {
                            &["--continue"]
                        } else {
                            &["--model", "haiku"]
                        };
                        let planned = f
                            .plan("claude", args, &resume_env("claude", RESUME_ID, &token))
                            .unwrap();
                        planned.args.first() == Some(&OsString::from("--resume"))
                    })
                })
                .collect();
            let resumed = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|r| *r)
                .count();
            let acks = f.telemetry.join(RESUME_ACKS_DIR);
            let success = acks.join(&token).exists();
            let refusal = acks.join(format!("{token}{REFUSED_SUFFIX}")).exists();
            assert!(success ^ refusal, "round {round}: one outcome");
            assert_eq!(resumed, usize::from(success), "round {round}");
        }
    }

    /// PRD #1790 stage 3 (audit S2): a call that fails after claiming the
    /// token publishes nothing, gives no arguments, reads pending (never a
    /// resume), and leaves the token consumed.
    #[test]
    fn a_failure_after_the_claim_is_pending_and_consumes_the_token() {
        let f = fixture();
        executable(&f.real.join("claude"));
        let request = resume_env("claude", RESUME_ID, "failed-token");
        test_seams::set_fail_publish(true);
        let failed = f.plan("claude", &["--model", "haiku"], &request);
        test_seams::set_fail_publish(false);
        assert_eq!(failed.unwrap().args, os(&["--model", "haiku"]));
        let held = open_acks(&f.shim, false).unwrap();
        assert_eq!(ack_state(&held, "failed-token"), AckState::Pending);
        let again = f.plan("claude", &["--model", "haiku"], &request).unwrap();
        assert_eq!(again.args, os(&["--model", "haiku"]));
        assert_eq!(ack_state(&held, "failed-token"), AckState::Pending);
    }

    /// PRD #1790 stage 3 (audit S1): the acknowledgements directory is held
    /// by descriptor, so replacing it with a link after it was opened does not
    /// redirect the claim or the outcome; and a link in its place is refused.
    #[test]
    fn a_replaced_acks_directory_does_not_redirect_the_outcome() {
        let f = fixture();
        let held = open_acks(&f.shim, true).unwrap();
        let acks = f.telemetry.join(RESUME_ACKS_DIR);
        let moved = f.telemetry.join("moved-acks");
        let elsewhere = f.telemetry.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::rename(&acks, &moved).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &acks).unwrap();
        let spec = crate::agent_registry::spec(&crate::event::AgentType::ClaudeCode).resume;
        let args = settle(&held, "held-token", "claude", spec, Some(RESUME_ID), &[]);
        assert_eq!(args, os(&["--resume", RESUME_ID]));
        assert!(moved.join("held-token").is_file());
        assert!(moved.join("held-token.claim").is_file());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(
            open_acks(&f.shim, true).is_err(),
            "a link where the directory was is refused"
        );
    }

    /// PRD #1790 stage 3 (audit S3): a minted token names its expiry; one past
    /// it, or one claiming an expiry too far away, claims and writes nothing;
    /// retirement removes the files of expired tokens only; and a full
    /// directory admits no new claim.
    #[test]
    fn token_lifetimes_bound_the_acknowledgements_directory() {
        let now = now_secs();
        let token = mint_token(now, 90).unwrap();
        assert!(is_resume_token(&token), "{token}");
        assert_eq!(token_expiry(&token), Some(now + 90));
        assert_eq!(token_expiry("restore-token"), None);
        assert!(token_usable(&token, now));
        assert!(!token_usable(&token, now + 91));
        assert!(!token_usable(
            &format!("{}-ab", now + 2 * MAX_TOKEN_LIFETIME_SECS),
            now
        ));
        assert!(token_usable("restore-token", now));
        assert_eq!(
            token_expiry(&mint_token(now, u64::MAX).unwrap()),
            Some(now + MAX_TOKEN_LIFETIME_SECS)
        );

        let f = fixture();
        executable(&f.real.join("claude"));
        let expired = format!("{}-0123", now - 5);
        let planned = f
            .plan(
                "claude",
                &["--model", "haiku"],
                &resume_env("claude", RESUME_ID, &expired),
            )
            .unwrap();
        assert_eq!(planned.args, os(&["--model", "haiku"]));
        assert_resume_env_removed(&planned);
        assert!(!f.telemetry.join(RESUME_ACKS_DIR).exists());

        let held = open_acks(&f.shim, true).unwrap();
        let old = format!("{}-aa", now - ACK_RETIRE_GRACE_SECS - 10);
        let recent = format!("{}-bb", now - 10);
        for name in [
            old.clone(),
            format!("{old}{CLAIM_SUFFIX}"),
            format!("{old}{REFUSED_SUFFIX}"),
            format!(".{old}.tmp"),
            recent.clone(),
            format!("{recent}{CLAIM_SUFFIX}"),
            "restore-token".to_string(),
        ] {
            held.create_new_owner_only(&name).unwrap();
        }
        assert_eq!(retire_expired(&held, now), 3);
        let left: std::collections::BTreeSet<String> =
            std::fs::read_dir(f.telemetry.join(RESUME_ACKS_DIR))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
        assert_eq!(
            left,
            [
                recent.clone(),
                format!("{recent}{CLAIM_SUFFIX}"),
                "restore-token".into()
            ]
            .into_iter()
            .collect()
        );

        for n in 0..MAX_ACK_ENTRIES {
            held.create_new_owner_only(&format!("filler-{n}")).unwrap();
        }
        let full = f
            .plan(
                "claude",
                &[],
                &resume_env("claude", RESUME_ID, &mint_token(now, 60).unwrap()),
            )
            .unwrap();
        assert!(full.args.is_empty(), "a full directory admits no claim");
    }

    /// A telemetry directory holding the launch shim scripts, and
    /// a directory with the real programs.
    struct Fixture {
        _root: tempfile::TempDir,
        telemetry: PathBuf,
        shim: PathBuf,
        real: PathBuf,
        home: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let telemetry = root.path().join("telemetry");
        let shim = install(&telemetry, "/opt/deck/dot-agent-deck").unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        for name in PROGRAMS {
            executable(&real.join(name));
        }
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        Fixture {
            telemetry,
            shim,
            real,
            home,
            _root: root,
        }
    }

    impl Fixture {
        fn path(&self) -> OsString {
            path_of(&[&self.shim, &self.real])
        }

        fn env(&self, pairs: &[(&str, &str)]) -> EnvMap {
            let mut all = vec![
                ("HOME", self.home.to_str().unwrap()),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                (RUN_ID_ENV, RUN),
            ];
            all.extend_from_slice(pairs);
            env(&all)
        }

        fn plan(&self, name: &str, args: &[&str], env: &EnvMap) -> Result<Plan, Refused> {
            plan(
                name,
                &self.shim,
                os(args),
                Some(&self.path()),
                None,
                Some(env),
                42,
            )
        }
    }

    /// Scenario: Install the launch shim and inspect its directory and scripts. Claude, Codex and Devin scripts are present with owner-only permissions.
    #[test]
    fn the_shim_directory_and_its_scripts_are_owner_only() {
        let f = fixture();
        for name in ["claude", "codex", "devin"] {
            assert!(f.shim.join(name).is_file(), "missing {name} shim");
        }
        assert_eq!(f.shim, f.telemetry.join(DIR_NAME));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&f.shim), 0o700);
        for name in PROGRAMS {
            assert_eq!(mode(&f.shim.join(name)), 0o700, "{name}");
        }
        assert_eq!(
            std::fs::read_dir(&f.shim).unwrap().count(),
            PROGRAMS.len(),
            "no staged file is left behind"
        );
        // A second daemon start rewrites them in place.
        install(&f.telemetry, "/other/deck").unwrap();
        let text = std::fs::read_to_string(f.shim.join("codex")).unwrap();
        assert!(
            text.contains("exec '/other/deck' __telemetry-shim codex '"),
            "{text}"
        );
        let colon = f.telemetry.parent().unwrap().join("a:b");
        assert!(install(&colon, "/d").is_err(), "PATH cannot carry it");
        assert!(!colon.exists());
    }

    /// The script hands every argument to the deck exactly as it came: run it
    /// with a "deck" that prints its argv.
    #[test]
    fn the_script_passes_the_arguments_through_untouched() {
        let root = tempfile::tempdir().unwrap();
        let deck = root.path().join("it's a deck");
        crate::test_isolation::write_script(
            &deck,
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&deck, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = install(&root.path().join("t"), deck.to_str().unwrap()).unwrap();
        let out = std::process::Command::new(dir.join("codex"))
            .envs(crate::test_isolation::unreachable_endpoints())
            .args(["a b", "", "$HOME", "--", "'q'"])
            .output()
            .unwrap();
        let lines: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                SUBCOMMAND,
                "codex",
                dir.to_str().unwrap(),
                "--",
                "a b",
                "",
                "$HOME",
                "--",
                "'q'"
            ]
        );
    }

    #[test]
    fn resolution_skips_the_shim_directory() {
        let f = fixture();
        assert_eq!(
            resolve("codex", Some(&f.path()), &f.shim),
            Some(f.real.join("codex"))
        );
        // Through a symlink to the shim directory, and a link to a script.
        let root = f.telemetry.parent().unwrap();
        let linked_dir = root.join("linked");
        std::os::unix::fs::symlink(&f.shim, &linked_dir).unwrap();
        let linked_file = root.join("linked-file");
        std::fs::create_dir(&linked_file).unwrap();
        std::os::unix::fs::symlink(f.shim.join("codex"), linked_file.join("codex")).unwrap();
        let path = path_of(&[&linked_dir, &linked_file, &f.shim, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
    }

    #[test]
    fn resolution_takes_the_first_executable_file_or_nothing() {
        let f = fixture();
        let root = f.telemetry.parent().unwrap();
        let plain = root.join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("codex"), "not executable").unwrap();
        let dir = root.join("dir");
        std::fs::create_dir_all(dir.join("codex")).unwrap();
        let earlier = root.join("earlier");
        std::fs::create_dir(&earlier).unwrap();
        executable(&earlier.join("codex"));
        let path = path_of(&[&f.shim, &plain, &dir, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        let path = path_of(&[&f.shim, &earlier, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(earlier.join("codex"))
        );
        assert_eq!(
            resolve("codex", Some(&path_of(&[&f.shim, &plain])), &f.shim),
            None
        );
        assert_eq!(resolve("codex", None, &f.shim), None);
    }

    #[test]
    fn a_missing_program_has_no_plan() {
        let f = fixture();
        let only_shim = path_of(&[&f.shim]);
        assert_eq!(
            plan(
                "codex",
                &f.shim,
                Vec::new(),
                Some(&only_shim),
                None,
                Some(&f.env(&[])),
                1
            ),
            Err(Refused::NotFound)
        );
        assert!(
            f.plan("codex", &[], &f.env(&[]))
                .is_ok_and(|p| p.program == f.real.join("codex"))
        );
    }

    #[test]
    fn codex_gets_its_exporters_in_front_of_its_own_arguments() {
        let f = fixture();
        let plan = f
            .plan("codex", &["exec", "--model", "m"], &f.env(&[]))
            .unwrap();
        assert_eq!(plan.program, f.real.join("codex"));
        let args: Vec<_> = plan.args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with("otel.trace_exporter="), "{args:?}");
        assert_eq!(args[2], "-c");
        assert!(args[3].starts_with("otel.metrics_exporter="), "{args:?}");
        assert_eq!(args[4..], ["exec", "--model", "m"]);
    }

    /// Issue #2002: the wrapper's embedded-mode pair stays in front of the
    /// exporters; the same arguments anywhere else get no such treatment.
    #[test]
    fn codex_keeps_the_embedded_pair_in_front_of_its_exporters() {
        let f = fixture();
        let [c, set] = crate::wrap::CODEX_EMBEDDED_ARGS;
        let strs = |plan: Plan| -> Vec<String> {
            plan.args
                .iter()
                .map(|a| a.to_str().unwrap().to_string())
                .collect()
        };
        let args = strs(
            f.plan("codex", &[c, set, "--model", "m"], &f.env(&[]))
                .unwrap(),
        );
        assert_eq!(args[..2], [c, set]);
        assert!(args[3].starts_with("otel.trace_exporter="), "{args:?}");
        assert!(args[5].starts_with("otel.metrics_exporter="), "{args:?}");
        assert_eq!(args[6..], ["--model", "m"]);

        let args = strs(
            f.plan("codex", &["--model", "m", c, set], &f.env(&[]))
                .unwrap(),
        );
        assert!(args[1].starts_with("otel.trace_exporter="), "{args:?}");
        assert_eq!(args[4..], ["--model", "m", c, set]);
    }

    /// Every way the decision declines runs the program with its own
    /// arguments, unchanged.
    #[test]
    fn a_declined_launch_runs_the_program_unchanged() {
        let f = fixture();
        let codex_home = f.home.join("alt");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "[otel]\nexporter = \"none\"\n",
        )
        .unwrap();
        let args = ["--yolo", "a b"];
        let unchanged = |name: &str, plan: Result<Plan, Refused>| {
            assert_eq!(
                plan,
                Ok(Plan {
                    program: f.real.join(name),
                    args: os(&args),
                    remove_env: Vec::new(),
                })
            );
        };
        for pairs in [
            &[("OTEL_SDK_DISABLED", "true")][..],
            &[
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none"),
            ][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "")][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://u:p@c.example")][..],
            &[("CODEX_HOME", codex_home.to_str().unwrap())][..],
        ] {
            unchanged("codex", f.plan("codex", &args, &f.env(pairs)));
        }
        for pairs in [
            &[("OTEL_SDK_DISABLED", "true")][..],
            &[(RUN_ID_ENV, "")][..],
            &[(RUN_ID_ENV, "../../etc")][..],
            &[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c?token=1")][..],
        ] {
            unchanged("devin", f.plan("devin", &args, &f.env(pairs)));
        }
        // The guard: this program was shimmed further up its process tree.
        let guard = OsString::from("devin:codex");
        assert_eq!(
            plan(
                "codex",
                &f.shim,
                os(&args),
                Some(&f.path()),
                Some(&guard),
                Some(&f.env(&[])),
                1
            ),
            Ok(Plan {
                program: f.real.join("codex"),
                args: os(&args),
                remove_env: Vec::new(),
            })
        );
        // A failure inside the decision: Devin's file cannot be written.
        std::fs::write(f.telemetry.join(format!("devin-{RUN}-42.json")), "taken").unwrap();
        unchanged("devin", f.plan("devin", &args, &f.env(&[])));
    }

    #[test]
    fn the_guard_names_only_what_was_shimmed() {
        let f = fixture();
        let codex_shimmed = OsString::from("codex");
        let devin = plan(
            "devin",
            &f.shim,
            Vec::new(),
            Some(&f.path()),
            Some(&codex_shimmed),
            Some(&f.env(&[])),
            1,
        )
        .unwrap();
        assert_eq!(devin.args[0], "--config", "a Devin run by Codex is shimmed");
        assert_eq!(guarded(None, "codex"), "codex");
        assert_eq!(guarded(Some(OsStr::new("")), "codex"), "codex");
        assert_eq!(guarded(Some(&codex_shimmed), "devin"), "codex:devin");
    }

    #[test]
    fn devin_gets_a_config_named_for_the_run_and_the_process() {
        let f = fixture();
        let plan = f.plan("devin", &["--yolo"], &f.env(&[])).unwrap();
        let config = f.telemetry.join(format!("devin-{RUN}-42.json"));
        assert_eq!(
            plan.args,
            [
                OsString::from("--config"),
                config.clone().into_os_string(),
                OsString::from("--yolo")
            ]
        );
        assert!(config.is_file());
        super::super::launch::remove_run_config_files(&f.telemetry, RUN);
        assert!(!config.exists(), "the daemon removes it with the run");
        assert!(f.shim.join("devin").exists(), "and leaves the shim alone");
    }
    /// Two deck installations on one `PATH`, and a shim script copied or
    /// hard-linked outside its directory: each is passed over, so the real
    /// program is found and nothing runs the shim again.
    #[test]
    fn resolution_skips_every_decks_shim_scripts() {
        let f = fixture();
        let root = f.telemetry.parent().unwrap();
        let other = install(&root.join("other-telemetry"), "/other/deck").unwrap();
        let path = path_of(&[&f.shim, &other, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        assert_eq!(
            resolve("codex", Some(&path), &other),
            Some(f.real.join("codex")),
            "and from the other installation's side"
        );
        let copied = root.join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::copy(f.shim.join("codex"), copied.join("codex")).unwrap();
        let linked = root.join("hard-linked");
        std::fs::create_dir(&linked).unwrap();
        std::fs::hard_link(f.shim.join("codex"), linked.join("codex")).unwrap();
        let path = path_of(&[&copied, &linked, &f.shim, &f.real]);
        assert_eq!(
            resolve("codex", Some(&path), &f.shim),
            Some(f.real.join("codex"))
        );
        assert!(is_shim_script(script("/d", "/x", "devin").as_bytes()));
        assert!(!is_shim_script(b"#!/bin/sh\nexec codex \"$@\"\n"));
    }

    /// A process tree that keeps coming back to the shim is refused once it
    /// has passed through it [`MAX_HOPS`] times, rather than run again.
    #[test]
    fn the_shim_refuses_a_launch_that_keeps_coming_back() {
        let f = fixture();
        let hops = |n: usize| OsString::from(vec!["codex"; n].join(":"));
        let run = |guard: &OsString| {
            plan(
                "codex",
                &f.shim,
                Vec::new(),
                Some(&f.path()),
                Some(guard),
                Some(&f.env(&[])),
                1,
            )
        };
        assert!(run(&hops(MAX_HOPS - 1)).is_ok());
        assert_eq!(run(&hops(MAX_HOPS)), Err(Refused::TooManyHops));
    }

    /// What the real program is decides whether the shim may add to it.
    #[test]
    fn the_recognised_forms_come_from_the_file_and_its_path() {
        for head in [
            &b"\x7fELF\x02\x01"[..],
            b"\xcf\xfa\xed\xfe....",
            b"\xca\xfe\xba\xbe....",
        ] {
            assert!(is_native(head), "{head:?}");
        }
        for head in [&b"#!/usr/bin/env node\n"[..], b"#!/bin/sh\n", b"ELF", b""] {
            assert!(!is_native(head), "{head:?}");
        }
        assert!(is_codex_npm_launcher(Path::new(
            "/usr/local/lib/node_modules/@openai/codex/bin/codex.js"
        )));
        assert!(is_codex_npm_launcher(Path::new(
            "/h/.pnpm/node_modules/.pnpm/@openai+codex@1/node_modules/@openai/codex/bin/codex.js"
        )));
        for path in [
            "/usr/local/lib/node_modules/@openai/codex/bin/other.js",
            "/usr/local/lib/node_modules/@openai/codex-wrapper/bin/codex.js",
            "/usr/local/lib/node_modules/codex/bin/codex.js",
            "/home/u/@openai/codex/bin/codex.js",
            "/usr/local/bin/codex",
        ] {
            assert!(!is_codex_npm_launcher(Path::new(path)), "{path}");
        }
        assert!(is_nix_store_path(Path::new(
            "/nix/store/0abc-codex-0.1/bin/codex"
        )));
        assert!(!is_nix_store_path(Path::new("/home/u/nix/store/codex")));
        assert!(!is_nix_store_path(Path::new("/nix/storefront/codex")));
    }

    /// A Nix store script is added to only when its text names none of the
    /// variables the decision reads, and is small enough to read in full.
    #[test]
    fn a_nix_store_script_that_names_a_decision_variable_gets_nothing() {
        let store = Path::new("/nix/store/0abc-codex-0.1/bin/codex");
        let plain: &[u8] = b"#!/nix/store/x-bash/bin/bash -e\nexport PATH='/nix/store/y/bin':$PATH\nexec -a \"$0\" /nix/store/z/bin/.codex-wrapped \"$@\"\n";
        for name in PROGRAMS {
            assert!(is_recognised(name, store, plain), "{name}");
        }
        assert!(
            !is_recognised("codex", Path::new("/opt/codex/bin/codex"), plain),
            "the same script outside the store"
        );
        for text in [
            &b"#!/bin/sh\nexport CODEX_HOME=/opted/out\nexec codex\n"[..],
            b"#!/bin/sh\nexport XDG_CONFIG_HOME=/x\n",
            b"#!/bin/sh\nHOME=/x exec codex\n",
            b"#!/bin/sh\nexport OTEL_TRACES_EXPORTER=none\n",
            b"no interpreter line\n",
        ] {
            assert!(
                !is_recognised("codex", store, text),
                "{}",
                String::from_utf8_lossy(text)
            );
        }
        let mut long = plain.to_vec();
        long.resize(SCRIPT_LIMIT as usize + 1, b'#');
        assert!(!is_recognised("codex", store, &long), "too long to read");
    }

    /// The real program as npm installs Codex: a `codex` on `PATH` that links
    /// to `bin/codex.js` in an `@openai/codex` package under `root`.
    fn npm_codex(root: &Path, on_path: &Path) {
        let package = root.join("lib/node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&package).unwrap();
        crate::test_isolation::write_script(package.join("codex.js"), "#!/usr/bin/env node\n")
            .unwrap();
        std::fs::set_permissions(
            package.join("codex.js"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let _ = std::fs::remove_file(on_path);
        std::os::unix::fs::symlink(package.join("codex.js"), on_path).unwrap();
    }

    /// A native program gets the additions, and so does npm's Codex
    /// launcher; any other script declines — a shell wrapper of the user's,
    /// one whose interpreter is a shell under another name, a Python wrapper,
    /// a Node script outside the `@openai/codex` package — because each may
    /// set `CODEX_HOME` to a configuration that opts out before running the
    /// real one. So do a file the shim cannot read and an environment the
    /// decision cannot read exactly.
    #[test]
    fn only_a_launcher_the_shim_is_sure_of_gets_the_additions() {
        let f = fixture();
        let root = f.telemetry.parent().unwrap();
        let args = ["exec"];
        let gets = |name: &str| f.plan(name, &args, &f.env(&[])).unwrap().args.len() > args.len();
        assert!(gets("codex"), "a native program");
        assert!(gets("devin"), "Devin's too");
        let wrapper = f.real.join("codex");
        npm_codex(root, &wrapper);
        assert!(gets("codex"), "npm's Codex launcher");
        std::fs::remove_file(&wrapper).unwrap();
        let shell = root.join("custom-shell");
        std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
        for (body, what) in [
            (
                "#!/bin/sh\nexport CODEX_HOME=/opted/out\nexec /opt/codex \"$@\"\n".to_string(),
                "a shell wrapper",
            ),
            (
                format!(
                    "#!{}\nexport CODEX_HOME=/opted/out\nexec /opt/codex \"$@\"\n",
                    shell.display()
                ),
                "a shell under another name",
            ),
            (
                "#!/usr/bin/env python3\nimport os\nos.environ['CODEX_HOME'] = '/opted/out'\n"
                    .to_string(),
                "a Python wrapper",
            ),
            (
                "#!/usr/bin/env node\nprocess.env.CODEX_HOME = '/opted/out';\n".to_string(),
                "a Node script outside the package",
            ),
        ] {
            crate::test_isolation::write_script(&wrapper, &body).unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!gets("codex"), "{what}");
        }
        npm_codex(root, &f.real.join("devin"));
        assert!(!gets("devin"), "npm's Codex launcher is Codex's only");
        executable(&wrapper);
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o111)).unwrap();
        if std::fs::File::open(&wrapper).is_err() {
            assert!(!gets("codex"), "unreadable");
        }
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let inexact = plan("codex", &f.shim, os(&args), Some(&f.path()), None, None, 1);
        assert_eq!(
            inexact,
            Ok(Plan {
                program: wrapper,
                args: os(&args),
                remove_env: Vec::new(),
            }),
            "an environment it cannot read exactly"
        );
    }
}
