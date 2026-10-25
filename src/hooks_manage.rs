use std::io::{self, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde_json::{Value, json};

use crate::agent_hook_config::{
    binary_names_match, executables_match, rule_command_strs, strip_deck_commands,
    unquote_if_needed,
};
// Exercised only by this module's own tests, which drive the platform-convention
// arithmetic with both hosts' conventions injected (see `binary_names_match_under`).
#[cfg(test)]
use crate::agent_hook_config::{binary_names_match_under, strip_suffix_ignoring_ascii_case};

const HOOK_TYPES: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Issue #714: the hook Claude Code fires INSTEAD of `Stop` when an API error
/// ends the turn — how a provider quota block reaches the deck. Installed with
/// no matcher, so every `error` kind arrives (a non-quota one maps to `Error`),
/// and only for a Claude Code that accepts it ([`STOP_FAILURE_MIN_CLAUDE_VERSION`]).
const STOP_FAILURE_HOOK: &str = "StopFailure";

/// Issue #714: the first Claude Code release that accepts a `StopFailure` key
/// under `hooks` in `settings.json`.
///
/// **An older release does not ignore the key: it drops EVERY hook in the
/// file.** Measured on 2026-09-26 with a sandboxed `HOME`, a `settings.json`
/// holding `SessionStart`, `UserPromptSubmit`, `Stop` and `StopFailure` command
/// hooks, and `claude -p`: 2.0.0, 2.1.50, 2.1.74, 2.1.76 and 2.1.77 ran none of
/// them, and each ran `SessionStart` and `UserPromptSubmit` once the
/// `StopFailure` key was removed; 2.1.78, 2.1.79, 2.1.85, 2.1.100, 2.1.200 and
/// 2.1.283 ran them with the key present. So the key is written only when the
/// installed Claude Code is known to be at least this version, and an unknown
/// version is treated as older.
pub const STOP_FAILURE_MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 78);

/// PRD #1631: the hook Claude Code runs when it is about to ask the user for
/// permission — a tool prompt, an `AskUserQuestion` form, a plan approval. Its
/// payload carries the question, and a decision printed by a hook that is still
/// running answers it, so the deck HOLDS this hook open until it has an answer
/// ([`crate::hook`]'s `CLAUDE_HOLD_DEADLINE`). Installed with no matcher, with
/// [`PERMISSION_REQUEST_HOOK_TIMEOUT_SECS`], and only for a Claude Code at least
/// [`PERMISSION_REQUEST_MIN_CLAUDE_VERSION`].
pub const PERMISSION_REQUEST_HOOK: &str = "PermissionRequest";

/// PRD #1631: the first Claude Code release the deck installs
/// [`PERMISSION_REQUEST_HOOK`] for.
///
/// The key exists from 2.0.45, but 2.1.136 is the release whose changelog
/// fixes `AskUserQuestion` discarding multi-select answers supplied as an
/// array — which is how the deck answers one. It is above
/// [`STOP_FAILURE_MIN_CLAUDE_VERSION`], so the "an older release drops every
/// hook given an unknown key" hazard that gate guards against (issue #714) is
/// covered here too without measuring the releases in between. An unknown
/// version is treated as older.
pub const PERMISSION_REQUEST_MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 136);

/// PRD #1631: the `timeout`, in seconds, the deck writes on its
/// `PermissionRequest` hook. Claude Code's default is 600 s, after which it
/// cancels the hook and goes ahead as if there were none — so a user who
/// leaves a prompt for longer still answers it by keyboard. An hour is a
/// choice, not a measured maximum.
pub const PERMISSION_REQUEST_HOOK_TIMEOUT_SECS: u64 = 3600;

/// The version-gated hook keys an install writes for one Claude Code (issues
/// #714 and PRD #1631).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClaudeHookGates {
    pub stop_failure: bool,
    pub permission_request: bool,
    /// The native inbox waiter (PRD #1666, [`CLAUDE_ASYNC_REWAKE_MIN`]).
    pub async_rewake: bool,
}

impl ClaudeHookGates {
    /// The gates for a Claude Code reporting `version`; `None` (unknown) opens
    /// none of them.
    pub fn for_version(version: Option<(u64, u64, u64)>) -> Self {
        Self {
            stop_failure: version.is_some_and(|v| v >= STOP_FAILURE_MIN_CLAUDE_VERSION),
            permission_request: version.is_some_and(|v| v >= PERMISSION_REQUEST_MIN_CLAUDE_VERSION),
            async_rewake: version.is_some_and(|v| v >= CLAUDE_ASYNC_REWAKE_MIN),
        }
    }

    /// Only the `StopFailure` gate, as the pre-PRD-#1631 seams take it.
    pub fn stop_failure_only(stop_failure: bool) -> Self {
        Self {
            stop_failure,
            ..Self::default()
        }
    }
}

/// PRD #1631: the gates for the `claude` on `PATH`, from the same
/// `claude --version` probe as [`installed_claude_accepts_stop_failure`], with
/// the raw first line for a message.
pub fn installed_claude_hook_gates() -> (ClaudeHookGates, Option<String>) {
    let (_, line) =
        probe_claude_version(std::ffi::OsStr::new("claude"), CLAUDE_VERSION_PROBE_TIMEOUT);
    let gates = ClaudeHookGates::for_version(line.as_deref().and_then(parse_claude_version));
    (gates, line)
}

/// How long [`installed_claude_accepts_stop_failure`] waits for
/// `claude --version` before treating the version as unknown. Since issue
/// #1157 this also runs in `daemon serve` BEFORE it binds, so it is part of the
/// daemon's pre-bind budget ([`crate::daemon_attach::DAEMON_START_POLL_TIMEOUT`]).
pub(crate) const CLAUDE_VERSION_PROBE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

/// PRD #1666 S6: the first Claude Code release the deck writes its native
/// inbox waiter for — an `asyncRewake` command handler on `SessionStart` and
/// `Stop` ([`WAITER_COMMAND_SUFFIX`]).
///
/// **This is the release the behaviour was MEASURED on, not a known minimum.**
/// The PRD #1666 spike (2026-10-10, Haiku) and the S6 live check ran 2.1.296: a
/// waiter that exits 2 wakes an idle session, a `timeout` of 86,400 s keeps an
/// idle waiter alive past the 600 s default, and pre-`/clear` waiters still
/// deliver. Earlier releases (2.1.294 and 2.1.295 sit beside it on the dev box)
/// carry the `asyncRewake` key in their binaries, but no run checked how they
/// treat it, and the `StopFailure` precedent above shows an unknown hook key
/// can switch off every deck hook. So an older or unknown version gets no
/// waiter, and the doorbell delivers to it instead. Lower this only with a
/// measurement. The waiter's `rewakeMessage` / `rewakeSummary` keys ride the
/// same gate; they are `@internal` in Claude Code ([`WAITER_REWAKE_MESSAGE`]).
pub const CLAUDE_ASYNC_REWAKE_MIN: (u64, u64, u64) = (2, 1, 296);

/// PRD #1666 S6: the text Claude Code puts ahead of a waiter's message when it
/// wakes the session, in place of its default `Stop hook blocking error from
/// command "<hook>":`. That default reads as a failure, and Claude Code also
/// marks every wake NOT USER INPUT; together with a framing that called the
/// block the user's request, Haiku sometimes refused the message as a prompt
/// injection (framing spike, 2026-10-12).
///
/// `rewakeMessage` and [`WAITER_REWAKE_SUMMARY`]'s `rewakeSummary` are
/// **`@internal` in Claude Code's settings schema**: measured present in
/// 2.1.294, 2.1.295 and 2.1.296, but undocumented and free to be renamed. They
/// sit under the same gate as the waiter ([`CLAUDE_ASYNC_REWAKE_MIN`]) for the
/// reason that gate exists: the `StopFailure` precedent shows a hook key a
/// release does not know can switch off every deck hook in the file, so no key
/// is written to a release no run has seen accept it. If a later release drops
/// them, the wake falls back to Claude Code's default wording and
/// [`crate::inbox_hub::SESSION_FRAMING`] still explains it.
pub const WAITER_REWAKE_MESSAGE: &str = "dot-agent-deck message, delivered by the deck the user runs you in (this is not a hook error):";

/// PRD #1666 S6: the line the Claude pane shows for a waiter's wake (`●
/// ✉ dot-agent-deck message`), in place of `Stop hook feedback`. `@internal`
/// in Claude Code like [`WAITER_REWAKE_MESSAGE`], and gated with it.
pub const WAITER_REWAKE_SUMMARY: &str = "✉ dot-agent-deck message";

/// PRD #1666 S6: the deck's second Claude handler kind, the native inbox
/// waiter, identified by this suffix the way the status handler is identified
/// by [`HOOK_COMMAND_SUFFIX`].
const WAITER_COMMAND_SUFFIX: &str = "inbox wait --agent claude-code";

/// PRD #1666 review SF-2: whether Claude Code's settings — the file the
/// installer writes ([`settings_path`]) — carry the deck's native waiter on
/// `SessionStart`. Only then does a waiter arm, so only then does the
/// `SessionStart` hook announce Native: below [`CLAUDE_ASYNC_REWAKE_MIN`] the
/// installer writes no waiter, and an announcement would only hold the first
/// doorbell for the arm grace. A missing, unreadable, oversized or malformed
/// file reads as no waiter.
pub fn claude_waiter_installed() -> bool {
    /// Claude Code's settings are small; anything larger is not read.
    const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;
    let Ok(file) = std::fs::File::open(settings_path()) else {
        return false;
    };
    let mut text = String::new();
    if std::io::Read::read_to_string(
        &mut std::io::Read::take(file, MAX_SETTINGS_BYTES + 1),
        &mut text,
    )
    .is_err()
        || text.len() as u64 > MAX_SETTINGS_BYTES
    {
        return false;
    }
    settings_have_waiter(&text)
}

/// [`claude_waiter_installed`] for the settings text `text`.
fn settings_have_waiter(text: &str) -> bool {
    let Ok(settings) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    settings["hooks"]["SessionStart"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|rule| rule["hooks"].as_array())
        .flatten()
        .filter_map(|handler| handler["command"].as_str())
        .any(|command| command.trim_end().ends_with(WAITER_COMMAND_SUFFIX))
}

/// The hook types that carry a waiter beside their status handler.
const WAITER_HOOK_TYPES: &[&str] = &["SessionStart", "Stop"];

/// The waiter's hook `timeout`, in seconds: one day. Claude Code's 600 s
/// default kills an idle waiter silently (spike §e); 86,400 s was observed
/// alive past it and is far below 2^31 ms.
pub const WAITER_TIMEOUT_SECS: u64 = 86_400;

/// Issue #714: parse the leading `MAJOR.MINOR.PATCH` of `claude --version`'s
/// output (`2.1.283 (Claude Code)`). `None` for anything else.
pub fn parse_claude_version(output: &str) -> Option<(u64, u64, u64)> {
    let word = output.split_whitespace().next()?;
    let mut parts = word.split('.');
    let mut next = || parts.next()?.parse::<u64>().ok();
    let version = (next()?, next()?, next()?);
    Some(version)
}

/// Issue #714: whether a Claude Code reporting `output` for `--version`
/// accepts the `StopFailure` hook key ([`STOP_FAILURE_MIN_CLAUDE_VERSION`]).
pub fn claude_version_accepts_stop_failure(output: &str) -> bool {
    parse_claude_version(output).is_some_and(|v| v >= STOP_FAILURE_MIN_CLAUDE_VERSION)
}

/// Issue #714: run `claude --version` (the `claude` on `PATH`, the one the deck
/// launches) and report whether it accepts the `StopFailure` hook key, with the
/// raw first line for a message. Not found, failed, timed out or unparseable
/// all answer `false`: writing the key for a Claude Code that rejects it would
/// switch off every deck hook, while leaving it out only means a quota-blocked
/// Claude Code agent is not shown as Blocked.
pub fn installed_claude_accepts_stop_failure() -> (bool, Option<String>) {
    probe_claude_version(std::ffi::OsStr::new("claude"), CLAUDE_VERSION_PROBE_TIMEOUT)
}

/// [`installed_claude_accepts_stop_failure`] with the program and the bound
/// injected.
///
/// **The whole probe sits under `timeout`, the pipe included** — not only the
/// child's exit. Waiting for the exit and then reading stdout to EOF would hang
/// for as long as anything else holds the pipe's write end: a launcher script
/// that backgrounds a helper, or a descendant of the version command, exits
/// the direct child while keeping stdout open, and this runs at TUI start with
/// [`SETTINGS_LOCK`] held. So stdout is read with the deadline
/// ([`read_version_line`]), stopping at the first newline, at EOF or after
/// 4 KiB. The line is accepted only when the child has also exited on its own
/// by then. Past the deadline, whether or not a line arrived, the probe answers
/// unknown, kills the child's whole tree — on Unix its process group, which it
/// was given at spawn, and on Windows the Job Object it is adopted into right
/// after spawn — and reaps it.
///
/// **Nothing the probe started outlives it on its side of the pipe** (issue
/// #1454). The read happens on the calling thread, so there is no reader to
/// abandon, and the read end is closed when the probe returns. A descendant
/// that escaped the tree — on Unix one that moved itself to another group, on
/// Windows one spawned in the instant between `CreateProcess` and the job
/// assignment ([`crate::platform::proc::AgentProcessGroup::adopt`] documents
/// that window) — keeps only its own write end, and its next write fails.
fn probe_claude_version(
    program: &std::ffi::OsStr,
    timeout: std::time::Duration,
) -> (bool, Option<String>) {
    let mut command = std::process::Command::new(program);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return (false, None),
    };
    // Windows has no process group to spawn into: contain the tree in a Job
    // Object instead, so `kill_probe` reaps a launcher's helpers too (Qodo on
    // PR #1346). Adoption failure degrades to killing the direct child only.
    #[cfg(windows)]
    let tree = crate::platform::proc::AgentProcessGroup::adopt(Some(child.id()));
    #[cfg(not(windows))]
    let tree = ProbeTree;
    let deadline = std::time::Instant::now() + timeout;
    let output = match child.stdout.take() {
        Some(mut stdout) => read_version_line(&mut stdout, deadline),
        None => Some(Vec::new()),
    };
    let Some(output) = output else {
        // Deadline passed with no line. The child is not reaped yet (a zombie
        // at worst), so its pid still names its group.
        kill_probe(&mut child, &tree);
        return (false, None);
    };
    // The line is in; it counts only if the child also exits on its own
    // within what is left of the deadline. A version command that prints a
    // plausible line and then hangs is killed and answers unknown, like one
    // that never printed: the line alone does not prove which Claude Code
    // `PATH` resolves to, and a wrong `true` disables every deck hook.
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                kill_probe(&mut child, &tree);
                return (false, None);
            }
        }
    }
    let output = String::from_utf8_lossy(&output);
    let line = output.lines().next().map(|l| l.trim().to_string());
    (
        line.as_deref()
            .is_some_and(claude_version_accepts_stop_failure),
        line,
    )
}

/// Read the version probe's stdout on the calling thread until a newline, EOF,
/// a read error or 4 KiB, whichever comes first, and hand back what was read
/// (at most 4 KiB). `None` when `deadline` passes first.
///
/// Every read is preceded by a wait for data that is itself bounded by the
/// deadline, so the read never blocks: on Unix `poll(2)` on the pipe, on
/// Windows `PeekNamedPipe` polled every 10ms (an anonymous pipe has no
/// overlapped reads to cancel).
fn read_version_line(
    stdout: &mut std::process::ChildStdout,
    deadline: std::time::Instant,
) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut output = Vec::new();
    let mut chunk = [0u8; 512];
    while output.len() < 4096 && !output.contains(&b'\n') {
        let available = match wait_for_probe_output(stdout, deadline) {
            ProbeOutput::Ready(available) => available.clamp(1, chunk.len()),
            ProbeOutput::Closed => break,
            ProbeOutput::TimedOut => return None,
        };
        match stdout.read(&mut chunk[..available]) {
            Ok(0) | Err(_) => break,
            Ok(n) => output.extend_from_slice(&chunk[..n]),
        }
    }
    output.truncate(4096);
    Some(output)
}

/// What [`wait_for_probe_output`] found on the pipe.
enum ProbeOutput {
    /// A read of up to this many bytes will not block: on Windows the count
    /// waiting in the pipe, on Unix `usize::MAX`, as a read after `poll(2)`
    /// reports the pipe readable returns what is there without blocking.
    Ready(usize),
    /// The pipe is broken or failed: treat as EOF.
    Closed,
    TimedOut,
}

/// Wait until the probe's stdout has something to read — data, EOF or an
/// error — or `deadline` passes.
#[cfg(unix)]
fn wait_for_probe_output(
    stdout: &std::process::ChildStdout,
    deadline: std::time::Instant,
) -> ProbeOutput {
    use std::os::fd::AsRawFd as _;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return ProbeOutput::TimedOut;
        }
        // Round up so a sub-millisecond remainder still waits rather than
        // spinning on a zero timeout.
        let millis = remaining.as_nanos().div_ceil(1_000_000);
        let timeout = libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX);
        let mut fd = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` for the duration of the call, on a
        // descriptor `stdout` owns and keeps open.
        let ready = unsafe { libc::poll(&mut fd, 1, timeout) };
        if ready > 0 {
            // POLLIN, POLLHUP and POLLERR all mean `read` returns at once:
            // data, EOF or the error.
            return ProbeOutput::Ready(usize::MAX);
        }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return ProbeOutput::Closed;
        }
        // Timed out or interrupted: the loop re-checks the deadline.
    }
}

/// [`wait_for_probe_output`] on Windows: poll `PeekNamedPipe`, which reports
/// the bytes waiting without blocking and fails once every write end is gone.
#[cfg(windows)]
fn wait_for_probe_output(
    stdout: &std::process::ChildStdout,
    deadline: std::time::Instant,
) -> ProbeOutput {
    use std::os::windows::io::AsRawHandle as _;
    loop {
        let mut available: u32 = 0;
        // SAFETY: `stdout` owns the handle and keeps it open for the call; no
        // buffer is passed, only the out-parameter for the available count.
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                stdout.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        // The deadline is checked before the result is used: bytes that
        // arrived during the last sleep came after it, and are as late as the
        // line a child prints after the deadline (Greptile on PR #1512).
        if std::time::Instant::now() >= deadline {
            return ProbeOutput::TimedOut;
        }
        if ok == 0 {
            return ProbeOutput::Closed;
        }
        if available > 0 {
            return ProbeOutput::Ready(available as usize);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Off Windows the probe's tree is its process group, named by the child's pid,
/// so there is nothing to hold.
#[cfg(not(windows))]
struct ProbeTree;

/// Kill a version probe that outlived its deadline — its whole tree first (the
/// process group on Unix, the Job Object on Windows) — and reap it. Must run
/// before the child is reaped, while its pid still names its group.
#[cfg(not(windows))]
fn kill_probe(child: &mut std::process::Child, _tree: &ProbeTree) {
    #[cfg(unix)]
    // SAFETY: `kill(2)` with a negative pid signals the process group that
    // `process_group(0)` gave the child, whose id is the child's pid. The
    // child is not reaped yet, so that id cannot have been reused.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// [`kill_probe`] on Windows: terminate the Job Object the probe was adopted
/// into, which reaps every process that joined it, then the direct child in
/// case adoption failed.
#[cfg(windows)]
fn kill_probe(child: &mut std::process::Child, tree: &crate::platform::proc::AgentProcessGroup) {
    tree.terminate_tree("claude version probe");
    let _ = child.kill();
    let _ = child.wait();
}

/// Every hook type the deck may have installed, gated or not — what uninstall
/// sweeps.
fn hook_types_all() -> Vec<&'static str> {
    hook_types(ClaudeHookGates {
        stop_failure: true,
        permission_request: true,
        async_rewake: true,
    })
}

/// The hook types to install: [`HOOK_TYPES`], plus [`STOP_FAILURE_HOOK`] when
/// the installed Claude Code accepts it.
fn hook_types(gates: ClaudeHookGates) -> Vec<&'static str> {
    let mut types = HOOK_TYPES.to_vec();
    if gates.stop_failure {
        types.push(STOP_FAILURE_HOOK);
    }
    if gates.permission_request {
        types.push(PERMISSION_REQUEST_HOOK);
    }
    types
}

/// Claude Code's user settings file, in the location Claude itself uses:
/// `~/.claude/settings.json`.
///
/// PRD #163 M1: resolved through the platform seam rather than a raw `$HOME`
/// read, so that on Windows — where `$HOME` is normally unset — this finds
/// `%USERPROFILE%\.claude` instead of missing it entirely.
///
/// PRD #163 review: the seam function is
/// [`crate::platform::paths::home_dir_with_tmp_fallback`], *not* `home_dir`,
/// because the raw read this replaced fell back to `/tmp` when `$HOME` was unset.
/// Unix behavior is therefore byte-for-byte what it was — including the
/// `/tmp/.claude/settings.json` an unset `$HOME` resolves to.
fn settings_path() -> PathBuf {
    crate::platform::paths::home_dir_with_tmp_fallback()
        .join(".claude")
        .join("settings.json")
}

/// Serializes the whole read-modify-write of `settings.json` across concurrent
/// in-process callers — two panes launching Claude Code at once, each running
/// [`auto_install`]. Combined with [`write_atomic`]'s temp-file+`rename`
/// publish, this closes the concurrent-clobber and partial-write window on the
/// user's real `~/.claude/settings.json` (issue #534), mirroring
/// `codex_hooks_manage::INSTALL_LOCK` (`:87`) and its findings #1/M-2.
///
/// Every public entry point in this module takes it for the FULL span from the
/// read to the write, not just around the write: locking the write alone still
/// lets two callers read the same "before" state and have the second one
/// overwrite the first's rule with a stale copy.
///
/// The cross-PROCESS lost update — two deck binaries starting at the same
/// instant — is closed by the advisory file lock [`lock_settings`] takes beside
/// it (`agent_hook_config::lock_config`). A human's editor save takes no such
/// lock and can still race a deck; the atomic publish means the loser of that
/// race loses a whole update rather than leaving a torn file behind.
static SETTINGS_LOCK: Mutex<()> = Mutex::new(());

/// Take [`SETTINGS_LOCK`], recovering from a poisoned mutex rather than
/// panicking: a previous caller panicking mid-install says nothing about
/// whether the settings file is usable now, and the read is re-done from disk
/// under the guard regardless.
fn lock_settings(path: &Path) -> io::Result<SettingsGuard> {
    let thread = SETTINGS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Issue #1493's follow-up: the mutex keeps this process's threads apart;
    // the file lock keeps other deck processes out of the same read-merge-
    // publish, which otherwise lost the earlier writer's changes.
    let process = crate::agent_hook_config::lock_config(path)?;
    Ok(SettingsGuard {
        _process: process,
        _thread: thread,
    })
}

/// [`lock_settings`] for an INSTALL, which may be the first write ever: the
/// settings directory is created before the lock is taken, because a lock beside
/// a directory that does not exist yet is no lock at all — two first installs
/// would each read empty settings and the later publish would drop the earlier
/// one's hooks (Qodo on PR #1523). Uninstall keeps [`lock_settings`] and
/// creates nothing.
fn lock_settings_for_install(path: &Path) -> io::Result<SettingsGuard> {
    if let Some(parent) = path.parent().filter(|parent| !parent.is_dir()) {
        // PRD #1487: before the directory. A missing directory means a missing
        // settings file, so this install is never a no-op and would be refused
        // by `write_settings` anyway.
        crate::config_write_guard::ensure_config_write_allowed(path)?;
        std::fs::create_dir_all(parent)?;
    }
    lock_settings(path)
}

/// [`lock_settings`]'s two locks, released together. The file lock is declared
/// first so it is dropped first, before the mutex.
struct SettingsGuard {
    _process: crate::agent_hook_config::ConfigLock,
    _thread: MutexGuard<'static, ()>,
}

/// Read `path` the STRICT way used by every install AND uninstall path. Only a
/// MISSING file (`ErrorKind::NotFound`) means empty — mirroring
/// `codex_hooks_manage::install_to`'s contract (`:290-316`). Malformed JSON is
/// backed up next to the original (`<path>.bak`, leaving the original bytes on
/// disk untouched) and returned as an `Err` so every caller skips the write
/// instead of silently collapsing the user's settings to `{}` — the old
/// behavior here mapped ANY parse error to an empty config, so a settings.json
/// invalidated by a single trailing comma came back with `model`, `env`, and
/// every `permissions` entry destroyed, while the run reported success.
///
/// The copy aside goes through
/// [`agent_hook_config::backup_malformed`](crate::agent_hook_config::backup_malformed)
/// rather than a bare `std::fs::write`, which followed a symlink planted at that
/// predictable `.bak` name (#731). It never replaces a `settings.json.bak` that
/// is already there — most plausibly the user's own copy from before the edit
/// that broke the file (#537) — and the message names a backup only when it
/// holds these bytes.
///
/// Issue #522: this used to be the install path's reader only, with a
/// `read_settings_lenient` twin still serving [`uninstall`] and
/// [`uninstall_from`] — which called [`write_settings`] UNCONDITIONALLY on
/// whatever it returned, so an unparseable settings.json was truncated to `{}`
/// and rewritten on every uninstall run while the run reported success and
/// exited 0. The lenient reader is gone; there is one contract for both
/// directions now.
fn load_settings_or_refuse(path: &Path) -> io::Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(value),
            Err(parse_err) => {
                let backup = crate::agent_hook_config::backup_malformed(path, &bytes);
                Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "{} is not valid JSON (left unchanged, original {}): {parse_err}",
                        path.display(),
                        crate::agent_hook_config::preserved_phrase(&backup)
                    ),
                ))
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e),
    }
}

/// Publish `settings` to `path`, atomically and without widening or redirecting
/// anything (issue #534). The caller is expected to already hold
/// [`lock_settings`].
///
/// The old body was `std::fs::write` — `open(O_TRUNC)` then `write`, over a file
/// two deck processes and a human editor all write. Three exposures followed,
/// and the second and third are what MANUFACTURE the malformed settings.json
/// that [`load_settings_or_refuse`] now refuses to clobber: Claude Code reading
/// inside the window between the truncate and the write sees an empty file; a
/// crash between them leaves one on disk permanently.
fn write_settings(path: &Path, settings: &Value) -> io::Result<()> {
    // PRD #1487: before the directory and the temp file exist.
    crate::config_write_guard::ensure_config_write_allowed(path)?;
    if let Some(parent) = path.parent() {
        // `create_dir_all("")` is a documented no-op, so a bare relative
        // filename (whose parent is `""`) needs no special case here.
        std::fs::create_dir_all(parent)?;
    }
    refuse_symlinked_destination(path)?;
    let contents = serde_json::to_string_pretty(settings)?;
    write_atomic(path, contents.as_bytes())
}

/// Refuse when `path` is a symlink — typically `~/.claude/settings.json`
/// stowed into a dotfiles checkout.
///
/// There is no safe silent branch here. A `rename(2)` publish onto the link
/// path replaces the symlink with a regular file, orphaning the dotfiles copy
/// so the user's edits and the deck's silently diverge from that point on.
/// Resolving the link instead means writing through it to a path outside the
/// directory the deck was pointed at — the write-anywhere hazard a
/// same-directory publish exists to close, and the reason the temp file below
/// is never placed in a resolved target's directory. Refusing is the only
/// branch that destroys nothing: the error reaches the shell on an explicit
/// `hooks install` / `hooks uninstall`, and `deck.log` on the startup
/// auto-install (issue #91 covers making that startup refusal visible).
///
/// `symlink_metadata` does not follow the final component, so this is the one
/// stat that can see the link itself. A dangling symlink is caught too — it is
/// the same arrangement with the target not checked out yet.
fn refuse_symlinked_destination(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} is a symlink (left unchanged): the deck will not replace it with a \
                 regular file, nor write through it to a path outside {}. Point it at a \
                 real file, or edit the linked file's hooks by hand.",
                path.display(),
                path.parent().unwrap_or(Path::new(".")).display()
            ),
        )),
        _ => Ok(()),
    }
}

/// Write `bytes` to a temp file in `dest`'s OWN directory and `rename` it over
/// `dest`. Same directory means same filesystem, which is what makes the rename
/// atomic — a reader sees either the whole old file or the whole new one, never
/// a truncated one, and a crash leaves the old file intact with at most a stray
/// temp beside it.
///
/// Mode is taken from the destination, or 0600 when the file is new. This is
/// the same fix #360 applied to the Devin adapter and #382 then applied to the
/// Codex one — both now share it via [`crate::agent_hook_config::write_atomic`],
/// which this deliberately does not use (see that module's header: only this
/// copy publishes through `create_new`). It is load-bearing precisely BECAUSE
/// this switches to a rename: `File::create` applies
/// `0666 & !umask` — 0644 under a typical 022 umask, 0664 under 002 — and the
/// rename would then replace the destination with that wider file. An in-place
/// `std::fs::write` never had that exposure, so publishing atomically without
/// this would trade one defect for another over a file holding the user's `env`
/// and `permissions`.
fn write_atomic(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json");
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));

    let mut file = match create_new(&tmp) {
        Ok(file) => file,
        // A leftover temp from a crashed run under this same pid — or anything
        // else squatting the name. `remove_file` unlinks a symlink rather than
        // following it, so taking the name this way cannot be turned into a
        // write to somewhere else.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            create_new(&tmp)?
        }
        Err(e) => return Err(e),
    };
    // Removes the temp file on every early return and on unwind (PRD #1487).
    let mut cleanup = crate::agent_hook_config::TempFileGuard::new(tmp.clone());

    let published = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dest)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();

    drop(file);
    published.and_then(|()| std::fs::rename(&tmp, dest))?;
    cleanup.disarm();
    Ok(())
}

/// `open(O_CREAT | O_EXCL | O_WRONLY)`: create the temp file or fail, never
/// truncate an existing one and never follow a symlink at that path.
fn create_new(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
}

/// The fixed command signature that identifies a deck-authored rule, mirroring
/// `codex_hooks_manage::HOOK_COMMAND_SUFFIX` (`:81`) and
/// `devin_hooks_manage::HOOK_COMMAND_SUFFIX` (`:70`). `--agent` defaults to
/// `CliAgent::ClaudeCode` (`src/main.rs:60-64`), so `<path> hook --agent
/// claude-code` is a valid invocation, behaviourally identical to the bare
/// `<path> hook` this used to write — writing the explicit form is what lets a
/// rule be identified by this SUFFIX alone, regardless of the executable path
/// (or its basename) preceding it.
const HOOK_COMMAND_SUFFIX: &str = "hook --agent claude-code";

/// The compiled crate's own binary name (`"dot-agent-deck"` — upstream and every
/// fork alike; the crate name itself is never renamed). Every hook rule written
/// before this fix carries exactly this as its executable's basename in the
/// LEGACY `<path> hook` shape — see [`is_legacy_deck_rule`].
const DEFAULT_BINARY_NAME: &str = env!("CARGO_PKG_NAME");

/// The deck's hook command for `binary_path`, as every rule [`make_rule`]
/// builds carries it.
fn hook_command(binary_path: &str) -> String {
    // PRD #1497: through the `DOT_AGENT_DECK_BIN` override wherever the hook
    // runs in a POSIX shell (`sh -c`, per Claude Code's hooks reference).
    format!(
        "{} {HOOK_COMMAND_SUFFIX}",
        crate::agent_hook_config::overridable_command_word(
            &shell_quote_if_needed(binary_path),
            cfg!(windows),
        )
    )
}

/// Build a rule object in the new hooks format:
/// `{ "hooks": [{"type": "command", "command": "..."}] }`
/// For Notification, adds a matcher for permission_prompt. With `waiter`, the
/// native inbox waiter follows the status handler in the same rule.
fn make_rule(binary_path: &str, hook_type: &str, waiter: bool) -> Value {
    let command = hook_command(binary_path);
    let mut handlers = vec![json!({
        "type": "command",
        "command": command
    })];
    if waiter {
        handlers.push(waiter_handler(&waiter_command_for(&command)));
    }

    if hook_type == "Notification" {
        json!({
            "matcher": "permission_prompt",
            "hooks": handlers
        })
    } else if hook_type == PERMISSION_REQUEST_HOOK {
        // PRD #1631: the hook waits for the daemon's answer, so it needs more
        // than Claude Code's default 600 s.
        json!({
            "hooks": [{
                "type": "command",
                "command": handlers[0]["command"],
                "timeout": PERMISSION_REQUEST_HOOK_TIMEOUT_SECS,
            }]
        })
    } else {
        json!({
            "hooks": handlers
        })
    }
}

/// PRD #1666 S6: the waiter command naming the same executable, override
/// wrapper included, as the status command `status_command`.
fn waiter_command_for(status_command: &str) -> String {
    let word = status_command
        .strip_suffix(HOOK_COMMAND_SUFFIX)
        .unwrap_or(status_command);
    format!("{word}{WAITER_COMMAND_SUFFIX}")
}

/// PRD #1666 S6: the native waiter's handler object. `asyncRewake` makes an
/// exit 2 wake the session; the day-long `timeout` keeps an idle waiter alive
/// ([`WAITER_TIMEOUT_SECS`]); `rewakeMessage` and `rewakeSummary` replace
/// Claude Code's "Stop hook blocking error" wording ([`WAITER_REWAKE_MESSAGE`]).
fn waiter_handler(command: &str) -> Value {
    json!({
        "type": "command",
        "command": command,
        "asyncRewake": true,
        "timeout": WAITER_TIMEOUT_SECS,
        "rewakeMessage": WAITER_REWAKE_MESSAGE,
        "rewakeSummary": WAITER_REWAKE_SUMMARY
    })
}

/// PRD #1666 S6: the executable of a deck waiter command, still quoted as
/// written; `None` for anything that is not one.
fn waiter_format_executable(command: &str) -> Option<&str> {
    crate::agent_hook_config::command_executable(command, WAITER_COMMAND_SUFFIX)
}

/// Whether `command` is a deck waiter, of any install.
fn waiter_is_ours(command: &str) -> bool {
    waiter_format_executable(command).is_some()
}

/// Whether an install by `binary_path` owns the waiter `command`, by the same
/// rule [`command_is_deck_install`] applies to the status handler.
fn waiter_is_deck_install(command: &str, binary_path: &str) -> bool {
    waiter_format_executable(command).is_some_and(|exe| {
        let exe = unquote_if_needed(exe);
        executables_match(&exe, binary_path)
            || crate::agent_hook_config::is_replaceable_deck_install(&exe, binary_path)
    })
}

/// Single-quote `path` for a POSIX shell only when it contains a character
/// outside a conservative safe set; otherwise return it unchanged. Mirrors
/// `devin_hooks_manage::shell_quote_if_needed` (`:232-245`, tested by
/// `install_quotes_a_binary_path_with_spaces`, `:726-730`) — a binary path
/// containing whitespace (e.g. `/Applications/My Deck/dot-agent-deck`) written
/// unquoted splits into extra shell tokens and the command no longer parses to
/// the intended argv.
///
/// Claude Code invokes the hook command line via the platform's native shell —
/// `cmd.exe` on Windows, a different shell with different quoting rules — so
/// this is `#[cfg(unix)]` here; see the `#[cfg(windows)]` sibling below. POSIX
/// single quotes are not a `cmd.exe` quoting mechanism: `cmd.exe` treats `'`
/// as a literal character, so wrapping a Windows path in single quotes names a
/// file that does not exist rather than quoting it.
#[cfg(unix)]
fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b'%' | b','
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// Double-quote `path` for `cmd.exe` only when it contains a character outside
/// a conservative safe set; otherwise return it unchanged. `~` is NOT special
/// to `cmd.exe` (unlike POSIX, where it triggers home-directory expansion), so
/// it is in the safe set here — a real Windows temp path such as
/// `C:\Users\RUNNER~1\...\dot-agent-deck` needs no quoting at all.
///
/// `%` and `!` are excluded from the safe set, but excluding them does NOT
/// resolve them — the same species of over-claiming comment corrected as H3
/// on [`read_settings_lenient`] above. `cmd.exe` expands `%VAR%` even *inside*
/// double quotes, and `!VAR!` under delayed expansion; wrapping the path in
/// quotes here changes neither. What quoting actually buys is limited to
/// spaces and the other characters outside the safe set — a path containing a
/// literal `%` or `!` is written through with that character unresolved,
/// quoted or not.
#[cfg(windows)]
fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'\\' | b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b',' | b'~'
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("\"{}\"", path.replace('"', "\\\""))
    }
}

/// Ensure `settings["hooks"]` is an object and return a mutable reference to it.
fn ensure_hooks_object(settings: &mut Value) -> &mut serde_json::Map<String, Value> {
    let obj = settings
        .as_object_mut()
        .expect("settings must be an object");
    if !obj.contains_key("hooks") || !obj["hooks"].is_object() {
        obj.insert("hooks".into(), json!({}));
    }
    obj.get_mut("hooks").unwrap().as_object_mut().unwrap()
}

/// Ensure `hooks_obj[hook_type]` is an array and return a mutable reference.
fn ensure_hook_array<'a>(
    hooks_obj: &'a mut serde_json::Map<String, Value>,
    hook_type: &str,
) -> &'a mut Vec<Value> {
    if !hooks_obj.contains_key(hook_type) || !hooks_obj[hook_type].is_array() {
        hooks_obj.insert(hook_type.into(), json!([]));
    }
    hooks_obj
        .get_mut(hook_type)
        .unwrap()
        .as_array_mut()
        .unwrap()
}

/// What one [`install_impl`] pass did: which hook types it changed, which were
/// already current, and how many deck-owned commands it removed.
///
/// `repaired` is what makes PRD #381 M4's self-heal *observable* in the log.
/// Whether anything is published is decided by comparing the settings before
/// and after the pass instead (PRD #1487), so a pass that only pruned is still
/// written and a pass that changed nothing is not.
struct InstallOutcome {
    installed: Vec<&'static str>,
    skipped: Vec<&'static str>,
    /// Deck-owned commands removed: a duplicate install's command
    /// consolidated away, a stale pin, or any deck rule left under a hook type
    /// the deck no longer installs. Never a user-authored command — every
    /// predicate is gated on deck ownership first.
    repaired: usize,
    /// The binary the installed hook types name afterwards: `binary_path`,
    /// unless an automatic install kept another live durable install's entry.
    named: String,
}

#[cfg(test)]
fn install_impl(
    settings: &mut Value,
    binary_path: &str,
    with_stop_failure: bool,
) -> InstallOutcome {
    install_impl_gated(
        settings,
        binary_path,
        ClaudeHookGates::stop_failure_only(with_stop_failure),
    )
}

/// [`install_impl`] with every version gate spelled out.
fn install_impl_gated(
    settings: &mut Value,
    binary_path: &str,
    gates: ClaudeHookGates,
) -> InstallOutcome {
    install_impl_in(
        settings,
        binary_path,
        gates,
        crate::agent_hook_config::InstallMode::Explicit,
    )
}

/// [`install_impl_gated`] with the install mode spelled out. Under
/// [`crate::agent_hook_config::InstallMode::Automatic`] a deck command naming
/// another live, durable install is kept as the hook type's one entry, rule and
/// all, and a hook type with none gets that install's command — the policy the
/// Codex writer documents (PRD #1487).
fn install_impl_in(
    settings: &mut Value,
    binary_path: &str,
    gates: ClaudeHookGates,
    mode: crate::agent_hook_config::InstallMode,
) -> InstallOutcome {
    use crate::agent_hook_config::{InstallMode, KeptDeckEntry};

    let hook_types = hook_types(gates);
    let deck_owned = |cmd: &str| {
        command_is_deck_install(cmd, binary_path) || waiter_is_deck_install(cmd, binary_path)
    };
    let hooks_obj = ensure_hooks_object(settings);

    // Clean up deck entries for hook types no longer installed. These are
    // gone from the list entirely, so any deck rule there is stale regardless
    // of which binary wrote it — use the generic, binary-agnostic predicate.
    // Issue #714: that includes a deck `StopFailure` rule left behind for a
    // Claude Code that no longer accepts it (a downgrade), which would
    // otherwise switch off every hook in the file.
    //
    // Binary-agnostic on purpose, and unlike the Codex and Devin sweeps, which
    // take only their own install's rules (issue #730) — decided in issue #537
    // (item 4.5). Codex ignores an event key it does not know; Claude Code
    // drops every hook in the file over one. A build cannot tell whether a key
    // it does not install is one the installed Claude Code accepts, so it does
    // not keep another install's rule there, even a live sibling's whose
    // command it keeps under every type it does install. The cost, accepted:
    // two coexisting deck builds whose type lists differ remove each other's
    // rules for the types only one installs, until that one installs again.
    let mut repaired = 0usize;
    let all_keys: Vec<String> = hooks_obj.keys().cloned().collect();
    for key in all_keys {
        if !hook_types.contains(&key.as_str()) {
            if let Some(arr) = hooks_obj.get_mut(&key).and_then(|v| v.as_array_mut()) {
                repaired += strip_deck_commands(arr, command_is_ours);
            }
            // Remove the key entirely if the array is now empty
            if hooks_obj
                .get(&key)
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty())
            {
                hooks_obj.remove(&key);
            }
        }
    }

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    // Only current-format commands are kept: a legacy `<path> hook` rule is a
    // retired shape, migrated whoever wrote it.
    let kept = |rules: &[Value]| {
        crate::agent_hook_config::auto_install_kept_entry(
            rules,
            binary_path,
            |cmd| command_is_deck_install(cmd, binary_path),
            |cmd| current_format_executable(cmd).map(|exe| unquote_if_needed(exe).into_owned()),
            hook_command,
        )
    };
    let keeper = match mode {
        InstallMode::Explicit => None,
        InstallMode::Automatic => hook_types.iter().find_map(|hook_type| {
            hooks_obj
                .get(*hook_type)
                .and_then(Value::as_array)
                .and_then(|rules| kept(rules))
        }),
    };

    for &hook_type in &hook_types {
        let rules = ensure_hook_array(hooks_obj, hook_type);
        let before = rules.clone();
        let waiter_here = gates.async_rewake && WAITER_HOOK_TYPES.contains(&hook_type);
        let deck_commands_before = rule_command_strs(rules)
            .into_iter()
            .filter(|cmd| deck_owned(cmd))
            .count();

        let kept_here = match mode {
            InstallMode::Explicit => None,
            InstallMode::Automatic => kept(rules),
        };
        let mut expected = make_rule(binary_path, hook_type, waiter_here);
        let own_command = expected["hooks"][0]["command"]
            .as_str()
            .expect("make_rule writes a command")
            .to_string();
        let command = KeptDeckEntry::command_for(kept_here.as_ref(), keeper.as_ref(), &own_command)
            .to_string();
        let waiter_command = waiter_command_for(&command);
        expected["hooks"][0]["command"] = Value::String(command.clone());
        if waiter_here {
            expected["hooks"][1]["command"] = Value::String(waiter_command.clone());
        }
        // Another install's rule keeps its `matcher` and its place (PRD #1487);
        // only its command is rebuilt, around the same executable (PRD #1497).
        let keeps_other = matches!(kept_here, Some(KeptDeckEntry::Other { .. }));

        // ONE deck rule per hook type (PRD #1487), shared with the Codex and
        // Devin writers: the first deck command — this binary's, a legacy
        // rule, or any other install of the deck under its basename, live or
        // dead — is refreshed where it sits and every other copy is
        // consolidated away, leaving the user's handlers where they were.
        // Before this, a second still-valid install kept its own rule and
        // every hook event was delivered once per rule (issue #1171).
        match crate::agent_hook_config::consolidate_deck_handlers_in_place(rules, &command, |cmd| {
            command_is_deck_install(cmd, binary_path)
        }) {
            Some((rule_idx, _)) => {
                // A rule holding only the deck's commands — its status handler
                // and, since PRD #1666, its waiter — is the deck's own: rewrite
                // it whole so its `matcher` is the current one (the
                // `Notification` rule carries one). A rule the user shares is
                // left alone apart from the commands.
                let deck_only = !keeps_other
                    && rules[rule_idx]
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|handlers| {
                            handlers.iter().all(|handler| {
                                handler
                                    .get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(deck_owned)
                            })
                        })
                    && rules[rule_idx].get("command").is_none();
                if deck_only {
                    rules[rule_idx] = expected;
                }
            }
            None => {
                strip_deck_commands(rules, |cmd| command_is_deck_install(cmd, binary_path));
                strip_deck_commands(rules, |cmd| waiter_is_deck_install(cmd, binary_path));
                rules.push(expected);
            }
        }

        // PRD #1666 S6: the second deck handler kind. Exactly one waiter where
        // the gate allows it, consolidated the same way as the status handler;
        // none anywhere else, of any install — a waiter left for a Claude Code
        // that no longer passes the gate is the risk the gate exists for.
        if waiter_here {
            match crate::agent_hook_config::consolidate_deck_handlers_in_place(
                rules,
                &waiter_command,
                |cmd| waiter_is_deck_install(cmd, binary_path),
            ) {
                Some((rule_idx, handler_idx)) => {
                    if let Some(handler) = rules[rule_idx]
                        .get_mut("hooks")
                        .and_then(Value::as_array_mut)
                        .and_then(|handlers| handlers.get_mut(handler_idx))
                        .and_then(Value::as_object_mut)
                    {
                        // Every key but the command, so a waiter an older
                        // build wrote gains the ones added since.
                        if let Value::Object(wanted) = waiter_handler(&waiter_command) {
                            for (key, value) in wanted {
                                if key != "command" {
                                    handler.insert(key, value);
                                }
                            }
                        }
                    }
                }
                None => {
                    strip_deck_commands(rules, |cmd| waiter_is_deck_install(cmd, binary_path));
                    let status_rule = rules.iter_mut().find(|rule| {
                        rule.get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|handlers| {
                                handlers.iter().any(|handler| {
                                    handler.get("command").and_then(Value::as_str)
                                        == Some(command.as_str())
                                })
                            })
                    });
                    match status_rule
                        .and_then(|rule| rule.get_mut("hooks"))
                        .and_then(Value::as_array_mut)
                    {
                        Some(handlers) => handlers.push(waiter_handler(&waiter_command)),
                        None => rules.push(json!({ "hooks": [waiter_handler(&waiter_command)] })),
                    }
                }
            }
        } else {
            strip_deck_commands(rules, waiter_is_ours);
        }

        let deck_commands_after = rule_command_strs(rules)
            .into_iter()
            .filter(|cmd| deck_owned(cmd))
            .count();
        repaired += deck_commands_before.saturating_sub(deck_commands_after);
        if *rules == before {
            skipped.push(hook_type);
        } else {
            installed.push(hook_type);
        }
    }

    InstallOutcome {
        installed,
        skipped,
        repaired,
        named: KeptDeckEntry::named_binary(keeper.as_ref(), binary_path),
    }
}

/// Outcome of [`uninstall_impl`]: which hook types had at least one deck
/// command removed, and the total number of individual commands removed across
/// all of them. Reporting the actual count is what makes "matched nothing"
/// distinguishable from "removed some" — a message that always reads
/// "No dot-agent-deck hooks found to remove." is correct-sounding but silently
/// wrong the moment the matcher goes blind: it prints on every run whether or
/// not anything was actually there.
///
/// The count is of COMMANDS, not rule objects, since [`strip_deck_commands`]
/// removes one command at a time and a rule the user shares with the deck
/// survives with the deck's command taken out of it.
struct UninstallOutcome {
    hook_types: Vec<&'static str>,
    commands_removed: usize,
}

fn uninstall_impl(settings: &mut Value) -> UninstallOutcome {
    let hooks = match settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(h) => h,
        None => {
            return UninstallOutcome {
                hook_types: Vec::new(),
                commands_removed: 0,
            };
        }
    };

    let mut hook_types = Vec::new();
    let mut commands_removed = 0;

    for hook_type in hook_types_all() {
        if let Some(arr) = hooks.get_mut(hook_type).and_then(|v| v.as_array_mut()) {
            let removed = strip_deck_commands(arr, command_is_ours);
            if removed > 0 {
                hook_types.push(hook_type);
                commands_removed += removed;
            }
        }
    }
    // Issue #714: an emptied `StopFailure` key goes too — an older Claude Code
    // rejects the key itself, empty or not. PRD #1631: so does an emptied
    // `PermissionRequest`, for the same reason.
    for gated in [STOP_FAILURE_HOOK, PERMISSION_REQUEST_HOOK] {
        if hooks
            .get(gated)
            .and_then(|v| v.as_array())
            .is_some_and(|a| a.is_empty())
        {
            hooks.remove(gated);
        }
    }

    UninstallOutcome {
        hook_types,
        commands_removed,
    }
}

// --- Identify deck rules by command SUFFIX, not by basename ---
//
// The old matcher looked for the literal substring "dot-agent-deck" in a rule's
// command, which blinds it the moment the binary runs under any other filename
// (this fork's `worker-agent-deck`, or any other renamed build) — the rule it
// just wrote becomes invisible to the tool that wrote it. A second revision
// relocated the hardcoding into a basename FRAGMENT check instead, which failed
// destructively on a crate rename (a crate named `dot-x` derives fragment `"x"`,
// and uninstall then deletes unrelated user hooks) and could only migrate a
// legacy rule when the INSTALLING binary's own name looked deck-ish, which made
// a coexisting fork/upstream install silently delete each other's rules.
//
// The replacement identifies a rule by its command's exact SUFFIX,
// [`HOOK_COMMAND_SUFFIX`] — the same question the Codex and Devin adapters ask,
// which since issue #730 they ask through the one shared parse,
// [`crate::agent_hook_config::command_executable`], rather than through their own
// copies of it. No
// basename check is layered on top: the suffix `"hook --agent claude-code"` is
// specific enough on its own that an unrelated `mytool hook` or `git hook` (test
// 005) does not end with it, and a user hook that merely mentions the deck's
// name as an argument (test 004) does not either.
//
// The one basename check that remains is narrower and different in kind: a
// LEGACY rule (written before this fix, in the bare `<path> hook` shape with no
// `--agent` suffix) is recognised only when its own executable's basename is
// EXACTLY [`DEFAULT_BINARY_NAME`] — never a fragment, and never compared against
// the installing binary's name. See [`is_legacy_deck_rule`].

/// Parse `command` as `<executable> hook --agent claude-code` in the CURRENT
/// format, recovering the executable by parsing from the RIGHT
/// (`strip_suffix`), not by counting whitespace-split tokens — so a quoted (or,
/// historically, unquoted) executable path containing spaces still round-trips
/// (test 008). The returned token may still be shell-quoted; pass it through
/// [`unquote_if_needed`] before comparing it as a path.
fn current_format_executable(command: &str) -> Option<&str> {
    crate::agent_hook_config::command_executable(command, HOOK_COMMAND_SUFFIX)
}

/// Parse `command` as the LEGACY, pre-fix `<executable> hook` shape — no
/// `--agent` suffix, never quoted — recovering the executable the same
/// parse-from-the-right way as [`current_format_executable`], so a historical
/// unquoted spaced path (test 009) is still recoverable even though counting
/// whitespace-split tokens could not locate its executable.
fn legacy_format_executable(command: &str) -> Option<&str> {
    let exe = command.trim_end().strip_suffix("hook")?;
    let exe = exe.strip_suffix(' ')?;
    if exe.is_empty() { None } else { Some(exe) }
}

/// Whether `command` is a LEGACY deck rule: the bare `<executable> hook` shape,
/// where `executable`'s basename names [`DEFAULT_BINARY_NAME`] — the historical
/// default every rule was written under before this fix existed. Scoped to a
/// whole-basename match via [`binary_names_match`] (never a fragment, never the
/// installing binary's own name) so a user tool whose basename merely contains
/// "deck" (test 012) or ends in the literal word "hook" (test 005) is never
/// swept up.
///
/// Review finding H2: on Windows the installed binary carries the platform's
/// executable suffix (`dot-agent-deck.exe`), so every real legacy Windows rule
/// has a basename that never equals bare [`DEFAULT_BINARY_NAME`] byte for byte.
/// [`binary_names_match`] is what closes that gap, and is a no-op on Unix.
fn is_legacy_deck_rule(command: &str) -> bool {
    legacy_format_executable(command)
        .and_then(|exe| Path::new(exe).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|basename| binary_names_match(basename, DEFAULT_BINARY_NAME))
}

/// Whether `command` is a deck-owned hook command, generically — no specific
/// installing binary to compare against. True for any CURRENT-format command
/// (any executable, by the suffix alone) or any LEGACY-format command whose
/// executable is exactly [`DEFAULT_BINARY_NAME`].
fn command_is_ours(command: &str) -> bool {
    current_format_executable(command).is_some()
        || is_legacy_deck_rule(command)
        || waiter_is_ours(command)
}

/// Whether `command` is a deck-owned hook command that should be treated as
/// belonging to the SPECIFIC binary currently installing: either a
/// current-format command whose executable matches `binary_path`
/// ([`executables_match`]), or a legacy rule — which always migrates to
/// whichever binary is installing now, regardless of that binary's own name —
/// migration is keyed off the legacy RULE, never the installer.
fn command_matches_binary(command: &str, binary_path: &str) -> bool {
    if let Some(exe) = current_format_executable(command) {
        return executables_match(&unquote_if_needed(exe), binary_path);
    }
    is_legacy_deck_rule(command)
}

/// Whether `command` is a deck-owned command sharing `binary_path`'s own basename
/// — [`binary_names_match`], so the host platform's executable-suffix and
/// case conventions decide what "same basename" means, which is what took this
/// from "closed on Unix by the coincidence of an empty `EXE_SUFFIX`" to closed
/// everywhere — whose executable is not a pin the deck would write — see
/// [`crate::platform::paths::pin_is_repairable`] for what that means and for
/// the one case that still gets the benefit of the doubt.
///
/// **"Not a pin the deck would write" is deliberately not "POSITIVELY KNOWN not
/// to be a usable durable pin"**, which is what this said until issue #1027
/// (item 6) checked it against
/// [`crate::platform::paths::pin_is_repairable`]'s four true-cases. Only one of
/// the four — an absolute path the OS answers `Ok(false)` for — is a positive
/// determination about the file. A bare or relative pin is judged with no
/// filesystem access at all, and is replaceable because it is *cwd-dependent*,
/// not because it fails to run: issue #536 is about such a pin running, through
/// the agent's `$PATH`, as a binary nobody chose. A live `target/{debug,release}`
/// path works this minute and is replaceable for not being durable. The
/// direction of the gate is unchanged; the epistemic claim was one notch too
/// strong, and [`crate::agent_hook_config::pin_is_dead_sibling`] carries the
/// same correction so the two read alike.
///
/// `owned_command_executable` returns `None` for any command that is not
/// deck-owned by either shape, so what reaches the predicate is only a rule the
/// deck itself would recognise as its own, and only when it looks like a stale
/// sibling of the binary currently installing (same basename, different — now
/// unusable — path). A deck rule for a genuinely different-looking binary is
/// left to [`command_matches_binary`]/[`is_legacy_deck_rule`] instead, since
/// most of this file's own fixtures are fictional paths that were never on disk
/// to begin with and must not be swept up just because they don't exist.
///
/// **That is the narrow claim, and the wide one — "this can never prune a
/// user's own hook" — would be false.** It is the same overstatement PR #1029
/// narrowed on [`crate::agent_hook_config::pin_is_dead_sibling`], the shared
/// helper this function's own gate became, and the two are written to read
/// alike on purpose. Ownership upstream is the *suffix*, and the suffix is a
/// convention, not a capability: a user-authored command that deliberately ends
/// in the deck's verb is indistinguishable from a deck entry under it — which is
/// the whole premise of issue #730. Such a command, under this binary's own
/// basename, with a pin `pin_is_repairable` rejects, IS pruned here. The two
/// conjuncts keep that case rare rather than impossible.
///
/// Review finding: pruning on a bare `exists()` conflates "confirmed absent"
/// with "could not determine" — a working binary behind an unmounted volume,
/// or a path this process cannot `stat` (permissions), both make `exists()`
/// return `false` even though the executable is fine, and deleting a working
/// user's hook is worse than leaving a stale rule. That fail-safe direction
/// survives, and now lives in [`crate::platform::paths::pin_is_repairable`]
/// together with the rest of the "is this pin still usable" question.
///
/// **PRD #381 audit, MEDIUM-1: `try_exists` alone was not that question.** It
/// resolves a BARE `dot-agent-deck` — precisely what issue #536 describes the
/// old code writing — **relative to the process cwd**, so launching the deck
/// from any directory holding a file of that name made the bare pin look alive
/// and this returned `false`, leaving the rule in place beside the new durable
/// one. `/bin/sh` then resolves that persisted bare name through the AGENT's
/// `$PATH` at hook-fire time, which is #536's arbitrary-execution vector
/// surviving the fix that claims to close it. A relative path, a directory, a
/// non-executable file and a live `target/{debug,release}` path all passed the
/// same gate. [`crate::platform::paths::pin_is_repairable`] asks for the whole
/// invariant a freshly resolved path satisfies instead, and gives the
/// stat-error benefit of the doubt only to a well-formed absolute pin.
///
/// This does not fully resolve the underlying nondeterminism, and is not
/// meant to: it relocates the same class of problem one layer down rather
/// than removing it. The predicate still assumes `exe` is itself a bare
/// filesystem path; a command whose "executable" token is actually an
/// argv-prefixed wrapper invocation (not a literal path) will still report a
/// confident-looking "missing" for a string that was never a real path to
/// begin with, the same way [`executables_match`]'s `canonicalize` call can
/// already fail to resolve a path for reasons unrelated to the binary's
/// health. That gap is unchanged by this fix.
/// Whether an install by `binary_path` owns `command` under a hook type it
/// installs (PRD #1487): this binary's command or a legacy rule
/// ([`command_matches_binary`]), or any other install of the deck sharing its
/// basename, live or dead
/// ([`crate::agent_hook_config::is_replaceable_deck_install`]).
fn command_is_deck_install(command: &str, binary_path: &str) -> bool {
    command_matches_binary(command, binary_path)
        || owned_command_executable(command).is_some_and(|exe| {
            crate::agent_hook_config::is_replaceable_deck_install(&exe, binary_path)
        })
}

#[cfg(test)]
fn command_is_dead_deck(command: &str, binary_path: &str) -> bool {
    // The no-basename fail-safe (an empty or `..`-terminated installing path,
    // or a non-UTF-8 one prunes nothing) lives in `pin_is_dead_sibling`. The
    // old `Option == Option` comparison treated two `None`s as a MATCH, which
    // is the one direction that deletes a user's rule off a value nobody can
    // reason about.
    owned_command_executable(command)
        .is_some_and(|exe| crate::agent_hook_config::pin_is_dead_sibling(&exe, binary_path))
}

/// The literal, unquoted executable path a deck-owned command names, or `None`
/// if `command` is not deck-owned by either shape. Used only to check whether
/// that binary still exists on disk.
fn owned_command_executable(command: &str) -> Option<String> {
    if let Some(exe) = current_format_executable(command) {
        return Some(unquote_if_needed(exe).into_owned());
    }
    if is_legacy_deck_rule(command) {
        return legacy_format_executable(command).map(str::to_string);
    }
    None
}

/// Silently install hooks if Claude Code is detected.
/// Intended for dashboard startup — **never prints to stdout**, including on
/// the PRD #381 refusal path: the dashboard is already painting by the time
/// this can fail, so a refusal goes to `tracing::warn!` and nowhere else.
///
/// Returns what the hooks are pinned to afterwards (issue #1637), so the daemon
/// can name the binary Claude Code's events come from.
pub fn auto_install() -> Vec<crate::hook_binary::HookPin> {
    auto_install_to_gated(
        &settings_path(),
        crate::platform::paths::durable_binary_path,
        || installed_claude_hook_gates().0,
    )
    .into_iter()
    .collect()
}

/// [`auto_install`] for `hooks refresh` (PRD #1666 part 3, R-3): whether
/// Claude Code is installed here, and what the install could not read or
/// write, which [`auto_install`] only logs.
pub fn refresh_install() -> crate::hooks_refresh::InstallAttempt {
    refresh_install_at(
        &settings_path(),
        crate::platform::paths::durable_binary_path,
        || installed_claude_hook_gates().0,
    )
}

/// [`refresh_install`] against an explicit settings path, with the binary
/// resolver and the gates injected as [`auto_install_to_gated`] takes them.
fn refresh_install_at(
    path: &Path,
    resolve: impl FnOnce() -> Result<String, String>,
    gates: impl FnOnce() -> ClaudeHookGates,
) -> crate::hooks_refresh::InstallAttempt {
    let attempt = try_auto_install_to_gated(path, resolve, gates);
    if let Err(e) = &attempt {
        tracing::warn!("auto-install: {e}");
    }
    crate::hooks_refresh::InstallAttempt::of(attempt.transpose())
}

/// What Claude Code's hooks are pinned to now, read back from its settings
/// without installing anything (issue #1637's pin refresh,
/// `CLAUDE.configured_pins`). An empty list when the settings file is
/// missing, and `None` when its directory is (as at startup, which installs
/// nothing then) or the file cannot be read, which leaves the pins the daemon
/// knows.
pub fn configured_pins() -> Option<Vec<crate::hook_binary::HookPin>> {
    configured_pins_in(&settings_path())
}

/// [`configured_pins`] against an explicit settings path. Like
/// [`auto_install_to_gated`], nothing is read unless the settings directory
/// exists.
fn configured_pins_in(path: &Path) -> Option<Vec<crate::hook_binary::HookPin>> {
    if path.parent().is_none_or(|p| !p.exists()) {
        return None;
    }
    let Some(settings) = crate::agent_hook_config::read_json_config(path)? else {
        // Confirmed missing: it names nothing, which clears a notice about
        // the binary it used to name (Qodo on #1656).
        return Some(Vec::new());
    };
    Some(
        crate::agent_hook_config::configured_deck_executables(&settings, owned_command_executable)
            .into_iter()
            .map(|binary| crate::hook_binary::HookPin {
                agent: crate::event::AgentType::ClaudeCode,
                config: path.to_path_buf(),
                binary,
            })
            .collect(),
    )
}

/// [`auto_install`] against an explicit settings path, with the binary-path
/// resolver injected — the seam PRD #381 M3 exists to open.
///
/// This function used to take the path alone and hardcode
/// `let binary_path = "dot-agent-deck".to_string();`. That single line is the
/// structural reason the defect shipped: it is the seam tests drive, so **no
/// test ever executed the `current_exe()` derivation that produced the bad
/// value**, and the PRD calls closing it the highest-value milestone here.
/// Production passes [`crate::platform::paths::durable_binary_path`]; a test
/// passes a closure driving `durable_binary_path_with` with a
/// `…/target/release/dot-agent-deck` `current_exe()` of its own and asserts
/// that value never reaches the file.
///
/// The resolver is called only after the settings *directory* check, so the
/// common "Claude Code not installed" case still costs one `try_exists()` and
/// no filesystem walk.
///
/// Issue #714: this seam installs the base hook set, which every Claude Code
/// accepts, and never the version-gated `StopFailure`; [`auto_install_to_gated`]
/// is the same seam with the gate injected, and what [`auto_install`] calls.
pub fn auto_install_to(
    path: &Path,
    resolve: impl FnOnce() -> Result<String, String>,
) -> Option<crate::hook_binary::HookPin> {
    auto_install_to_gated(path, resolve, ClaudeHookGates::default)
}

/// [`auto_install_to`] with the `StopFailure` gate injected (issue #714):
/// `stop_failure` answers whether the installed Claude Code accepts that hook
/// key ([`installed_claude_accepts_stop_failure`] in production). Asked only
/// after the directory check and the binary resolution, so a machine without
/// Claude Code never runs the probe.
pub fn auto_install_to_gated(
    path: &Path,
    resolve: impl FnOnce() -> Result<String, String>,
    gates: impl FnOnce() -> ClaudeHookGates,
) -> Option<crate::hook_binary::HookPin> {
    try_auto_install_to_gated(path, resolve, gates).unwrap_or_else(|e| {
        tracing::warn!("auto-install: {e}");
        None
    })
}

/// [`auto_install_to_gated`], returning why it wrote nothing rather than
/// logging it: `Ok(None)` when Claude Code is not installed here.
fn try_auto_install_to_gated(
    path: &Path,
    resolve: impl FnOnce() -> Result<String, String>,
    gates: impl FnOnce() -> ClaudeHookGates,
) -> Result<Option<crate::hook_binary::HookPin>, String> {
    // Only a directory confirmed missing means Claude Code is not installed:
    // one that cannot be inspected is a failure to report (PRD #1666 part 3),
    // and nothing is written either way.
    let Some(dir) = path.parent() else {
        return Ok(None);
    };
    match dir.try_exists() {
        Ok(true) => {}
        Ok(false) => return Ok(None),
        Err(e) => return Err(format!("could not inspect {}: {e}", dir.display())),
    }

    // PRD #381 M6: no durable path means no write at all — not a bare
    // command name, not a build-artifact path that breaks later.
    let binary_path = resolve()?;
    crate::agent_hook_config::ensure_hook_path_is_shell_safe(&binary_path)
        .map_err(|e| e.to_string())?;

    let _guard = lock_settings(path).map_err(|e| e.to_string())?;
    let mut settings = load_settings_or_refuse(path).map_err(|e| e.to_string())?;
    let before = settings.clone();
    let outcome = install_impl_in(
        &mut settings,
        &binary_path,
        gates(),
        crate::agent_hook_config::InstallMode::Automatic,
    );

    // Equal settings are not a write (PRD #1487); a pass that only PRUNED is
    // still a change and is published — PRD #381 M4.
    let pin = crate::hook_binary::HookPin {
        agent: crate::event::AgentType::ClaudeCode,
        config: path.to_path_buf(),
        binary: outcome.named.clone(),
    };
    if settings == before {
        return Ok(Some(pin));
    }

    write_settings(path, &settings)
        .map_err(|e| format!("failed to write Claude Code hooks: {e}"))?;
    let binary_path = outcome.named.clone();
    crate::agent_hook_config::log_auto_install_change(
        "claude-code",
        path,
        &binary_path,
        "claude-code startup auto-install",
    );

    // Repair logs what it changed. Silently mutating global config is the same
    // class of thing that caused this bug, so a self-heal that leaves no trace
    // is not an acceptable fix for it.
    if outcome.repaired > 0 {
        tracing::info!(
            "repaired {} stale dot-agent-deck hook command(s) in {} (unusable binary pin or \
             retired hook type); now pinned to {binary_path}",
            outcome.repaired,
            path.display()
        );
    }
    if !outcome.installed.is_empty() {
        tracing::info!(
            "auto-installed Claude Code hooks: {}",
            outcome.installed.join(", ")
        );
    }
    Ok(Some(pin))
}

/// `dot-agent-deck hooks install --agent claude-code` — the explicit, chatty
/// install. Returns `Err` on a refused write (malformed settings.json left
/// unchanged, or the write itself failing) so the CLI dispatch in `main.rs`
/// reports failure on stderr AND exits non-zero, instead of a refusal
/// silently reporting success to the shell — see [`crate::agent_registry`]'s
/// `claude_install` adapter, which used to hardcode `Ok(())` here regardless
/// of outcome.
pub fn install() -> Result<(), String> {
    install_with(crate::platform::paths::durable_binary_path)
}

/// [`install`] with the binary-path resolution injected — the explicit-install
/// counterpart of the seam [`auto_install_to`] opens for the silent one.
///
/// PRD #381 M6: the resolution is the FIRST statement, so a refusal returns
/// before `settings_path()` is even computed. That is what makes "writes
/// nothing" a property of the code rather than a claim about it — no
/// truncation, no partial JSON, no created-then-abandoned file — and it is what
/// lets a test drive the refusal branch without a machine that genuinely has no
/// durable deck on it.
pub fn install_with(resolve: impl FnOnce() -> Result<String, String>) -> Result<(), String> {
    let binary_path = resolve()?;
    // Before the settings are read, so a refusal leaves them as they were.
    crate::agent_hook_config::ensure_hook_path_is_shell_safe(&binary_path)
        .map_err(|e| e.to_string())?;
    let (gates, claude_version) = installed_claude_hook_gates();

    let path = settings_path();
    let _guard = lock_settings_for_install(&path).map_err(|e| e.to_string())?;
    let mut settings = load_settings_or_refuse(&path).map_err(|e| e.to_string())?;

    let before = settings.clone();
    let InstallOutcome {
        installed, skipped, ..
    } = install_impl_gated(&mut settings, &binary_path, gates);

    // Equal settings are not rewritten (PRD #1487).
    if settings != before {
        write_settings(&path, &settings).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }

    if !installed.is_empty() {
        println!("Installed hooks: {}", installed.join(", "));
    }
    if !skipped.is_empty() {
        println!("Already installed (skipped): {}", skipped.join(", "));
    }
    if !gates.async_rewake {
        let (major, minor, patch) = CLAUDE_ASYNC_REWAKE_MIN;
        println!(
            "Note: dot-agent-deck's message waiter was not installed: `claude --version` \
             reported {}, and it is installed only for Claude Code {major}.{minor}.{patch} or \
             newer. Without it, deck messages reach a Claude Code agent through a one-line \
             notice typed into its pane instead of waking it directly. Run this again after \
             updating Claude Code.",
            claude_version.as_deref().unwrap_or("nothing usable")
        );
    }
    if !gates.stop_failure {
        let (major, minor, patch) = STOP_FAILURE_MIN_CLAUDE_VERSION;
        println!(
            "Note: the {STOP_FAILURE_HOOK} hook was not installed: `claude --version` reported \
             {}, and only Claude Code {major}.{minor}.{patch} or newer accepts it (an older \
             release ignores every hook in the file when it is present). Without it, a Claude \
             Code agent whose provider quota runs out is not shown as Blocked. Run this again \
             after updating Claude Code.",
            claude_version.as_deref().unwrap_or("nothing usable")
        );
    }
    if !gates.permission_request {
        let (major, minor, patch) = PERMISSION_REQUEST_MIN_CLAUDE_VERSION;
        println!(
            "Note: the {PERMISSION_REQUEST_HOOK} hook was not installed: `claude --version` \
             reported {}, and the deck installs it only for Claude Code {major}.{minor}.{patch} \
             or newer. Without it, a Claude Code agent's questions cannot be answered from the \
             desktop app. Run this again after updating Claude Code.",
            claude_version.as_deref().unwrap_or("nothing usable")
        );
    }
    println!("Settings file: {}", path.display());
    Ok(())
}

/// `dot-agent-deck hooks uninstall --agent claude-code`. Returns `Err` on a
/// refused read or a failed write, so the refusal reaches the shell — issue
/// #522, the uninstall half of #516. `agent_registry::claude_uninstall`
/// hardcoded `Ok(())` here regardless of outcome, so even a genuine write
/// failure could not reach the CLI's exit code; that is what #506 fixed for
/// `claude_install` alone.
pub fn uninstall() -> Result<(), String> {
    let path = settings_path();
    let _guard = lock_settings(&path).map_err(|e| e.to_string())?;
    let mut settings = load_settings_or_refuse(&path).map_err(|e| e.to_string())?;

    let outcome = uninstall_impl(&mut settings);

    if outcome.commands_removed == 0 {
        // Nothing of ours was in there, so there is nothing to publish. Writing
        // anyway would reserialize the whole file — reindenting and reordering
        // keys the deck does not own, and creating a `{}` settings.json on a
        // machine that never had one.
        println!("No dot-agent-deck hooks found to remove.");
    } else {
        write_settings(&path, &settings).map_err(|e| format!("writing {}: {e}", path.display()))?;
        println!(
            "Removed {} hook command{}: {}",
            outcome.commands_removed,
            if outcome.commands_removed == 1 {
                ""
            } else {
                "s"
            },
            outcome.hook_types.join(", ")
        );
    }
    println!("Settings file: {}", path.display());
    Ok(())
}

// --- Testable versions that accept a custom path ---

/// [`install`] against an explicit settings path. Returns the same refusals the
/// CLI path reports rather than swallowing them, so a test that expects a write
/// to be refused has to say so — a seam that quietly discarded the refusal is
/// how #522's uninstall defect stayed invisible under a green suite.
///
/// Issue #714: installs the base hook set, like [`auto_install_to`];
/// [`install_to_gated`] injects the `StopFailure` gate.
pub fn install_to(path: &Path, binary_path: &str) -> io::Result<()> {
    install_to_gated(path, binary_path, false)
}

/// [`install_to`], writing the version-gated `StopFailure` hook too when
/// `stop_failure` is set (issue #714).
pub fn install_to_gated(path: &Path, binary_path: &str, stop_failure: bool) -> io::Result<()> {
    install_to_with_gates(
        path,
        binary_path,
        ClaudeHookGates::stop_failure_only(stop_failure),
    )
}

/// [`install_to`] with every version gate given (PRD #1631).
pub fn install_to_with_gates(
    path: &Path,
    binary_path: &str,
    gates: ClaudeHookGates,
) -> io::Result<()> {
    crate::agent_hook_config::ensure_hook_path_is_shell_safe(binary_path)?;
    let _guard = lock_settings_for_install(path)?;
    let mut settings = load_settings_or_refuse(path)?;
    let before = settings.clone();
    install_impl_gated(&mut settings, binary_path, gates);
    if settings == before {
        return Ok(());
    }
    write_settings(path, &settings)
}

/// [`uninstall`] against an explicit settings path, with [`uninstall`]'s
/// contract: a file that cannot be read or parsed is left byte-for-byte as
/// found and reported as an error, and nothing is written when nothing of the
/// deck's was there.
pub fn uninstall_from(path: &Path) -> io::Result<()> {
    let _guard = lock_settings(path)?;
    let mut settings = load_settings_or_refuse(path)?;
    if uninstall_impl(&mut settings).commands_removed == 0 {
        return Ok(());
    }
    write_settings(path, &settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;

    /// Scenario (issue #1637): the daemon's pin refresh reads back what Claude
    /// Code's settings pin, writing nothing: no evidence for a missing
    /// settings directory or an unparseable file (which is left as it is), no
    /// pin for missing settings or settings holding no deck hook, and the
    /// binary the deck's hooks name once they are there.
    /// Scenario (PRD #1666 part 3, R-3): `hooks refresh` runs the startup
    /// install and must tell a Claude Code that is not installed from one
    /// whose settings it could not update. No settings directory is absent;
    /// malformed settings, which the install refuses and leaves as they were,
    /// and a refused binary path both ran and failed, with why; good settings
    /// ran and failed nothing.
    #[test]
    fn refresh_install_tells_not_installed_from_failed() {
        use crate::hooks_refresh::InstallAttempt;
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        let exe = crate::test_paths::abs("/opt/deck/dot-agent-deck");
        let resolve = || Ok(exe.clone());
        assert_eq!(
            refresh_install_at(
                &dir.path().join("absent").join("settings.json"),
                resolve,
                ClaudeHookGates::default
            ),
            InstallAttempt::Absent
        );
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        match refresh_install_at(&path, resolve, ClaudeHookGates::default) {
            InstallAttempt::Ran { failed } => assert_eq!(failed.len(), 1, "{failed:?}"),
            other => panic!("malformed settings must fail: {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            refresh_install_at(
                &path,
                || Err("no durable path".to_string()),
                ClaudeHookGates::default
            ),
            InstallAttempt::Ran {
                failed: vec!["no durable path".into()]
            }
        );
        assert_eq!(
            refresh_install_at(&path, resolve, ClaudeHookGates::default),
            InstallAttempt::Ran { failed: Vec::new() }
        );
        assert!(path.exists());
    }

    #[test]
    fn configured_pins_reads_the_settings_back_without_writing() {
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        assert_eq!(
            configured_pins_in(&dir.path().join("absent").join("settings.json")),
            None,
            "no settings directory, as at startup"
        );
        let path = dir.path().join("settings.json");
        assert_eq!(configured_pins_in(&path), Some(Vec::new()));
        assert!(!path.exists(), "a read-back creates nothing");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(configured_pins_in(&path), None);
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        assert!(!dir.path().join("settings.json.bak").exists());
        let user_only = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/true"}]}]}
        });
        std::fs::write(&path, user_only.to_string()).unwrap();
        assert_eq!(configured_pins_in(&path), Some(Vec::new()));
        let old = crate::test_paths::abs("/opt/old/dot-agent-deck");
        let old = old.as_str();
        let pinned = serde_json::json!({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": hook_command(old)}]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": hook_command(old)}]}]
            }
        });
        std::fs::write(&path, pinned.to_string()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let pins = configured_pins_in(&path).expect("readable settings");
        assert_eq!(
            pins,
            vec![crate::hook_binary::HookPin {
                agent: crate::event::AgentType::ClaudeCode,
                config: path.clone(),
                binary: old.to_string(),
            }]
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Scenario (Qodo on #1656): Claude Code's hooks are pinned to an older
    /// copy, which raised its notice, and then the user deletes
    /// `settings.json`. The next refresh reads it as naming nothing, so the
    /// notice clears within one refresh; a settings file that becomes
    /// unreadable instead keeps the pin and the notice.
    #[test]
    fn deleting_the_settings_clears_the_notice_and_a_read_error_keeps_it() {
        use crate::hook_binary::{DeckIdentity, HookBinaryState, PinRefresh};
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        let path = dir.path().join("settings.json");
        let old = dir.path().join("missing-old").join("dot-agent-deck");
        let old = old.to_string_lossy().into_owned();
        let pinned = serde_json::json!({
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": hook_command(&old)}]}]}
        });
        std::fs::write(&path, pinned.to_string()).unwrap();
        let deck = DeckIdentity {
            exe: None,
            version: "0.46.0".into(),
            self_install_path: None,
        };
        let pins = configured_pins_in(&path).expect("readable settings");
        // `old` does not exist, so its probe fails: an `Unprobeable` notice.
        let mut state = HookBinaryState::from_startup(deck.clone(), &pins, None);
        assert_eq!(state.notices().len(), 1);
        let refresh = |path: &Path| {
            PinRefresh::classify(
                &deck,
                configured_pins_in(path)
                    .map(|pins| vec![(crate::event::AgentType::ClaudeCode, pins)])
                    .unwrap_or_default(),
            )
        };

        #[cfg(unix)]
        {
            // A directory where the settings file should be: unreadable.
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert_eq!(configured_pins_in(&path), None);
            assert!(!state.apply_refresh(refresh(&path)));
            assert_eq!(state.notices().len(), 1, "a read error keeps the pin");
            std::fs::remove_dir(&path).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::remove_file(&path).unwrap();

        assert!(state.apply_refresh(refresh(&path)));
        assert!(state.notices().is_empty(), "{:?}", state.notices());
    }

    /// A malformed `settings.json` is copied to `settings.json.bak` — but never
    /// THROUGH a symlink planted at that path, and since #537 never over one
    /// either.
    ///
    /// The backup destination is fully predictable, and `std::fs::write` follows
    /// a symlink, so a writer able to add an entry to `~/.claude` could point
    /// `settings.json.bak` at any file it could write and have the deck fill
    /// that file with the malformed config's bytes (#731's second half). The
    /// victim here stands for that file.
    ///
    /// [`refuse_symlinked_destination`] does not cover this: it guards the
    /// publish destination, which this path never reaches — the whole point of
    /// the refusal is that the settings file is left unwritten.
    #[cfg(unix)]
    #[test]
    fn a_malformed_settings_backup_does_not_follow_a_symlink_planted_at_the_backup_path() {
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let malformed = "{ \"model\": \"opus\",, }";
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, malformed).expect("seed settings.json");
        let backup = dir.path().join("settings.json.bak");
        std::os::unix::fs::symlink(&victim, &backup).expect("plant symlink");

        let err = load_settings_or_refuse(&settings)
            .expect_err("settings we cannot parse must be refused, not collapsed to {}");
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the backup was written through the planted symlink and overwrote the victim"
        );
        assert!(
            std::fs::symlink_metadata(&backup)
                .expect("stat backup")
                .file_type()
                .is_symlink(),
            "something already at the backup name is left as it was (#537)"
        );
        assert!(
            !err.to_string().contains("preserved at"),
            "the planted link must not be claimed as the backup: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&settings).expect("read settings"),
            malformed,
            "the original must be left byte-for-byte intact"
        );
    }

    /// When the copy aside is not made, the error must not claim a backup.
    ///
    /// A directory at the `.bak` name is the portable way to occupy it; before
    /// #731 the message named `<path>.bak` unconditionally, because the write's
    /// result was discarded.
    #[test]
    fn a_backup_that_cannot_be_written_is_not_claimed_in_the_error() {
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, "{ nope").expect("seed settings.json");
        // Occupied by something no copy can be made over.
        std::fs::create_dir(dir.path().join("settings.json.bak")).expect("occupy the backup name");

        let err = load_settings_or_refuse(&settings).expect_err("malformed settings are refused");
        let message = err.to_string();

        assert!(
            !message.contains("preserved at"),
            "the error named a backup that was never written: {message}"
        );
        assert!(
            message.contains("not copied") || message.contains("not preserved"),
            "the error must say the copy aside did not happen: {message}"
        );
        assert!(
            message.contains("left unchanged"),
            "the original is still intact and the error must say so: {message}"
        );
    }

    /// The conventions [`binary_names_match`] reads off the target on Windows,
    /// spelled out so they can be exercised from a Linux or macOS box — the
    /// only kind available while PR #733's Windows-only defect was being
    /// fixed, since `aws-lc-sys` does not cross-compile.
    const WINDOWS: (&str, bool) = (".exe", true);
    /// The same for every Unix: no executable suffix, case-sensitive names.
    const UNIX: (&str, bool) = ("", false);

    fn matches(a: &str, b: &str, (suffix, insensitive): (&str, bool)) -> bool {
        binary_names_match_under(a, b, suffix, insensitive)
    }

    /// The Windows defect PR #733's second round exists to close: the
    /// installing path always carries `EXE_SUFFIX`
    /// (`platform::paths::durable_binary_file_name` appends it) while
    /// `DEFAULT_BINARY_NAME` never does, so a legacy bare pin and the binary
    /// installing over it are the same program spelled two ways.
    #[test]
    fn binary_names_match_across_the_windows_executable_suffix() {
        assert!(matches("dot-agent-deck", "dot-agent-deck.exe", WINDOWS));
        assert!(matches("dot-agent-deck.exe", "dot-agent-deck", WINDOWS));
        assert!(matches("dot-agent-deck.exe", "dot-agent-deck.exe", WINDOWS));
        assert!(matches("dot-agent-deck", "dot-agent-deck", WINDOWS));
        // Windows file names are case-insensitive, so this is the same binary
        // again — the review-finding-H2 half of the convention.
        assert!(matches("Dot-Agent-Deck.EXE", "dot-agent-deck.exe", WINDOWS));
    }

    /// The inversion guard. On Unix `foo.exe` is a file called `foo.exe` and
    /// nothing more, so stripping a hardcoded `".exe"` (or applying the
    /// case-insensitivity everywhere) must not compile away this difference.
    #[test]
    fn binary_names_stay_exact_on_unix() {
        assert!(matches("dot-agent-deck", "dot-agent-deck", UNIX));
        assert!(
            !matches("dot-agent-deck", "dot-agent-deck.exe", UNIX),
            "`.exe` is not this platform's executable suffix, so these are two \
             different file names"
        );
        assert!(
            !matches("Dot-Agent-Deck", "dot-agent-deck", UNIX),
            "Unix file names are case-sensitive"
        );
    }

    /// The safety property the basename gate exists for, at the unit level:
    /// installing `/a/dot-agent-deck` must never prune `/b/some-other-name`.
    /// Neither convention may collapse two genuinely different names.
    #[test]
    fn binary_names_never_match_a_different_program() {
        for conventions in [WINDOWS, UNIX] {
            assert!(!matches("some-other-name", "dot-agent-deck", conventions));
            assert!(!matches(
                "some-other-name.exe",
                "dot-agent-deck.exe",
                conventions
            ));
            // A fragment is not a match, in either direction — the property
            // `is_legacy_deck_rule` is scoped to a whole basename for.
            assert!(!matches(
                "dot-agent-deck-shim",
                "dot-agent-deck",
                conventions
            ));
            assert!(!matches("deck", "dot-agent-deck", conventions));
            // Only ONE suffix comes off, so a doubled one is still distinct.
            assert!(!matches(
                "dot-agent-deck.exe.exe",
                "dot-agent-deck",
                conventions
            ));
        }
    }

    /// The **call site**, not just the helper: `command_is_dead_deck` must
    /// recognise a legacy BARE pin as ours to repair when the binary installing
    /// over it is the deck's own installed file name on THIS platform.
    ///
    /// This is the assertion that failed on `build-windows` in PR #733's first
    /// round and passed everywhere else, because
    /// `platform::paths::durable_binary_file_name` appends `EXE_SUFFIX` while
    /// `DEFAULT_BINARY_NAME` — the literal the pre-fix code wrote as its
    /// `current_exe()` fallback — never does. A Windows machine carrying such a
    /// pin could therefore never be self-healed, so issue #536 was open there
    /// while looking closed. On Unix it held only by the coincidence that
    /// `EXE_SUFFIX` is empty.
    #[test]
    fn a_bare_pin_is_dead_against_the_platforms_own_installed_file_name() {
        let installing = format!(
            "/opt/dot-agent-deck/bin/{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        );
        assert!(
            command_is_dead_deck(
                &format!("{DEFAULT_BINARY_NAME} {HOOK_COMMAND_SUFFIX}"),
                &installing
            ),
            "a bare `{DEFAULT_BINARY_NAME}` pin must be repairable when \
             installing `{installing}` — /bin/sh (or cmd.exe) resolves that \
             persisted bare name through the AGENT's PATH at hook-fire time"
        );
        // And the legacy `<path> hook` shape, which reaches the same gate
        // through `is_legacy_deck_rule` rather than the current-format parse.
        assert!(command_is_dead_deck(
            &format!("{DEFAULT_BINARY_NAME} hook"),
            &installing
        ));
    }

    /// Scenario: Install Claude hooks from one live deck, then another at a different path. The second replaces the first and leaves exactly one command per event.
    #[test]
    fn a_live_same_named_deck_at_another_path_replaces_the_previous_install() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let a = fixture.path().join("first/dot-agent-deck");
        let b = fixture.path().join("second/dot-agent-deck");
        for binary in [&a, &b] {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            crate::test_isolation::write_script(binary, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(
                binary,
                <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
            )
            .unwrap();
        }
        let mut settings = serde_json::json!({});
        install_impl(&mut settings, a.to_str().unwrap(), false);
        install_impl(&mut settings, b.to_str().unwrap(), false);
        let commands = crate::agent_hook_config::rule_command_strs(
            settings["hooks"]["PreToolUse"].as_array().unwrap(),
        );
        assert_eq!(
            commands.len(),
            1,
            "one deck command per event: {commands:?}"
        );
        assert_eq!(
            commands[0],
            format!(
                "{} {HOOK_COMMAND_SUFFIX}",
                crate::agent_hook_config::overridable_command_word(
                    &shell_quote_if_needed(b.to_str().unwrap()),
                    cfg!(windows)
                )
            )
        );
    }

    /// The safety property the basename gate exists for, at its own call site
    /// and independently of the existence check: `pin_is_repairable` says
    /// "repairable" for every pin below on every platform — absent on Unix,
    /// and not absolute (no drive prefix) on Windows — so the basename
    /// comparison is the ONLY thing standing between them and deletion.
    /// Installing `/a/dot-agent-deck` must never prune `/b/some-other-name`'s
    /// rule.
    #[test]
    fn a_deck_rule_for_a_different_binary_is_never_dead_against_ours() {
        let installing = format!(
            "/opt/dot-agent-deck/bin/{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        );
        assert!(!command_is_dead_deck(
            &format!("/nowhere/some-other-name {HOOK_COMMAND_SUFFIX}"),
            &installing
        ));
        assert!(!command_is_dead_deck(
            &format!("/nowhere/{DEFAULT_BINARY_NAME}-shim {HOOK_COMMAND_SUFFIX}"),
            &installing
        ));
        // A user-authored command that merely MENTIONS the deck is not ours by
        // either shape, so it never reaches the basename gate at all.
        assert!(!command_is_dead_deck(
            "/opt/audit/wrapper --watch dot-agent-deck --report",
            &installing
        ));
    }

    /// A name that is nothing but the suffix keeps it: a file called `.exe` has
    /// that name, and stripping it to the empty string would make it match
    /// every other suffix-only name.
    #[test]
    fn a_suffix_only_name_is_not_stripped_to_nothing() {
        assert_eq!(strip_suffix_ignoring_ascii_case(".exe", ".exe"), ".exe");
        assert!(!matches(".exe", "", WINDOWS));
    }

    /// `strip_suffix_ignoring_ascii_case` slices by byte offset, so a basename
    /// ending in a multi-byte character can put the cut inside one. Slicing
    /// there panics, and a hook basename is arbitrary user-supplied text.
    #[test]
    fn stripping_never_panics_on_a_multibyte_boundary() {
        // 5 bytes, and `len - ".exe".len()` == 1 lands inside the first `é`.
        assert_eq!(strip_suffix_ignoring_ascii_case("ééa", ".exe"), "ééa");
        assert_eq!(strip_suffix_ignoring_ascii_case("é", ".exe"), "é");
        assert_eq!(strip_suffix_ignoring_ascii_case("", ".exe"), "");
        assert_eq!(strip_suffix_ignoring_ascii_case("déjà.EXE", ".exe"), "déjà");
    }

    /// The whole predicate, on the real host: whatever this platform's
    /// conventions are, the deck's own installed file name and
    /// `DEFAULT_BINARY_NAME` must name the same binary — that identity is what
    /// lets self-heal recognise a legacy pin as ours to repair, and it is the
    /// one assertion here that would have failed on `build-windows` before this
    /// fix.
    #[test]
    fn the_installed_file_name_and_the_default_name_are_the_same_binary() {
        let installed = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);
        assert!(binary_names_match(DEFAULT_BINARY_NAME, &installed));
        assert!(binary_names_match(&installed, DEFAULT_BINARY_NAME));
        assert!(!binary_names_match("some-other-name", &installed));
    }

    /// Scenario: The deck's `PermissionRequest` hook — the one it holds open
    /// to answer a question — is written with its long `timeout` for Claude
    /// Code 2.1.136 or newer, and for nothing older or unknown, which would
    /// drop every hook in the file over an unknown key. A downgrade removes a
    /// stale one, and uninstall sweeps it with its emptied key.
    #[spec("hooks/install/018")]
    #[test]
    fn hooks_install_018_permission_request_is_version_gated() {
        assert_eq!(
            ClaudeHookGates::for_version(Some((2, 1, 136))),
            ClaudeHookGates {
                stop_failure: true,
                permission_request: true,
                async_rewake: false,
            }
        );
        assert_eq!(
            ClaudeHookGates::for_version(Some((2, 1, 135))),
            ClaudeHookGates {
                stop_failure: true,
                permission_request: false,
                async_rewake: false,
            }
        );
        assert_eq!(
            ClaudeHookGates::for_version(None),
            ClaudeHookGates::default()
        );

        let binary = "/opt/deck/dot-agent-deck";
        let mut settings = serde_json::json!({"hooks": {}});
        let outcome = install_impl_gated(
            &mut settings,
            binary,
            ClaudeHookGates::for_version(parse_claude_version("2.1.289 (Claude Code)")),
        );
        assert!(outcome.installed.contains(&PERMISSION_REQUEST_HOOK));
        let rules = settings["hooks"][PERMISSION_REQUEST_HOOK]
            .as_array()
            .expect("a PermissionRequest rule");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].get("matcher"), None, "every tool's request");
        assert_eq!(
            rules[0]["hooks"][0]["timeout"],
            PERMISSION_REQUEST_HOOK_TIMEOUT_SECS
        );
        assert!(
            rules[0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(command_is_ours)
        );
        // Idempotent: a second install with the same gates leaves the same file.
        let before = settings.clone();
        install_impl_gated(
            &mut settings,
            binary,
            ClaudeHookGates::for_version(Some((2, 1, 289))),
        );
        assert_eq!(settings, before);

        for older in [Some((2, 1, 135)), None] {
            let mut fresh = serde_json::json!({});
            install_impl_gated(&mut fresh, binary, ClaudeHookGates::for_version(older));
            assert!(
                fresh["hooks"].get(PERMISSION_REQUEST_HOOK).is_none(),
                "{older:?} must not get the key"
            );
        }
        let outcome = install_impl_gated(
            &mut settings,
            binary,
            ClaudeHookGates::for_version(Some((2, 1, 100))),
        );
        assert!(
            outcome.repaired >= 1,
            "the stale rule is removed on a downgrade"
        );
        assert!(settings["hooks"].get(PERMISSION_REQUEST_HOOK).is_none());

        install_impl_gated(
            &mut settings,
            binary,
            ClaudeHookGates::for_version(Some((2, 1, 289))),
        );
        let removed = uninstall_impl(&mut settings);
        assert!(removed.hook_types.contains(&PERMISSION_REQUEST_HOOK));
        assert!(settings["hooks"].get(PERMISSION_REQUEST_HOOK).is_none());
        assert!(
            crate::hook::CLAUDE_HOLD_DEADLINE.as_secs() < PERMISSION_REQUEST_HOOK_TIMEOUT_SECS,
            "the held hook gives up before Claude Code cancels it"
        );
    }

    /// PRD #1666 part 2 (wire audit W06): `hooks install` from another build
    /// rewrites the prompt hook and both waiters together, so no mix of an
    /// older prompt hook and a newer waiter (or the reverse) survives it.
    #[test]
    fn install_rewrites_the_prompt_hook_and_the_waiters_as_one_unit() {
        let gates = ClaudeHookGates {
            stop_failure: false,
            permission_request: false,
            async_rewake: true,
        };
        let old = crate::test_paths::abs("/opt/old/dot-agent-deck");
        let new = crate::test_paths::abs("/opt/new/dot-agent-deck");
        let mut settings = serde_json::json!({});
        install_impl_gated(&mut settings, &old, gates);
        install_impl_gated(&mut settings, &new, gates);
        let commands = |hook_type: &str| -> Vec<String> {
            settings["hooks"][hook_type]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|rule| rule["hooks"].as_array().cloned().unwrap_or_default())
                .filter_map(|handler| handler["command"].as_str().map(str::to_string))
                .collect()
        };
        let prompt = commands("UserPromptSubmit");
        assert_eq!(prompt, [hook_command(&new)]);
        for hook_type in WAITER_HOOK_TYPES {
            let all = commands(hook_type);
            let waiters: Vec<&String> = all
                .iter()
                .filter(|command| command.ends_with(WAITER_COMMAND_SUFFIX))
                .collect();
            assert_eq!(waiters.len(), 1, "{hook_type}: {all:?}");
            assert_eq!(*waiters[0], waiter_command_for(&hook_command(&new)));
            assert!(
                all.iter().all(|command| !command.contains(old.as_str())),
                "{hook_type} keeps nothing of the older build: {all:?}"
            );
        }
    }

    /// A waiter added beside a status handler the deck shares with a user's
    /// hook goes into that shared rule, never into a rule of the user's own
    /// that holds no deck handler.
    #[test]
    fn a_new_waiter_joins_the_rule_holding_the_status_handler() {
        let gates = ClaudeHookGates {
            stop_failure: false,
            permission_request: false,
            async_rewake: true,
        };
        let binary = crate::test_paths::abs("/opt/deck/dot-agent-deck");
        let status = hook_command(&binary);
        let mut settings = serde_json::json!({"hooks": {}});
        for hook_type in WAITER_HOOK_TYPES {
            settings["hooks"][*hook_type] = serde_json::json!([
                {"hooks": [{"type": "command", "command": "notify-send done"}]},
                {"hooks": [
                    {"type": "command", "command": "logger turn"},
                    {"type": "command", "command": status.clone()}
                ]}
            ]);
        }

        install_impl_gated(&mut settings, &binary, gates);

        for hook_type in WAITER_HOOK_TYPES {
            let rules = settings["hooks"][*hook_type].as_array().expect("rules");
            let commands = |rule: &Value| -> Vec<String> {
                rule["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|handler| handler["command"].as_str().map(str::to_string))
                    .collect()
            };
            assert_eq!(
                commands(&rules[0]),
                ["notify-send done"],
                "{hook_type}: the user's own rule gained a handler"
            );
            assert_eq!(
                commands(&rules[1]),
                [
                    "logger turn".to_string(),
                    status.clone(),
                    waiter_command_for(&status)
                ],
                "{hook_type}: the waiter belongs beside the status handler"
            );
        }
    }

    /// Issue #714: the `StopFailure` gate — written for a Claude Code that
    /// accepts it, withheld (and a stale one removed) for one that does not,
    /// and swept by uninstall. The end-to-end half is `status/blocked/011` in
    /// `crate::hook`.
    #[test]
    fn stop_failure_hook_is_installed_only_for_a_claude_code_that_accepts_it() {
        assert_eq!(
            parse_claude_version("2.1.283 (Claude Code)"),
            Some((2, 1, 283))
        );
        assert!(claude_version_accepts_stop_failure("2.1.78 (Claude Code)"));
        assert!(claude_version_accepts_stop_failure("3.0.0"));
        assert!(!claude_version_accepts_stop_failure("2.1.77 (Claude Code)"));
        assert!(!claude_version_accepts_stop_failure(
            "1.0.128 (Claude Code)"
        ));
        assert!(!claude_version_accepts_stop_failure("Claude Code"));
        assert!(!claude_version_accepts_stop_failure(""));

        let binary = "/opt/deck/dot-agent-deck";
        let deck_rules = |settings: &Value, key: &str| {
            settings["hooks"][key]
                .as_array()
                .map(|rules| {
                    rule_command_strs(rules)
                        .into_iter()
                        .filter(|c| command_is_ours(c))
                        .count()
                })
                .unwrap_or(0)
        };
        let mut settings = serde_json::json!({
            "model": "opus",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}]}
        });
        let outcome = install_impl(&mut settings, binary, true);
        assert!(outcome.installed.contains(&STOP_FAILURE_HOOK));
        assert_eq!(deck_rules(&settings, STOP_FAILURE_HOOK), 1);
        assert_eq!(
            settings["hooks"][STOP_FAILURE_HOOK][0].get("matcher"),
            None,
            "no matcher: every error kind must reach the deck"
        );
        for hook_type in HOOK_TYPES {
            assert_eq!(deck_rules(&settings, hook_type), 1, "{hook_type}");
        }
        assert_eq!(settings["model"], "opus", "unrelated settings survive");

        // A Claude Code that predates the key: none written, the stale one gone.
        let outcome = install_impl(&mut settings, binary, false);
        assert!(!outcome.installed.contains(&STOP_FAILURE_HOOK));
        assert!(outcome.repaired >= 1);
        assert!(settings["hooks"].get(STOP_FAILURE_HOOK).is_none());

        let mut fresh = serde_json::json!({});
        install_impl(&mut fresh, binary, false);
        assert!(fresh["hooks"].get(STOP_FAILURE_HOOK).is_none());

        // Uninstall sweeps the gated key as well, and drops it once empty.
        install_impl(&mut settings, binary, true);
        let removed = uninstall_impl(&mut settings);
        assert!(removed.hook_types.contains(&STOP_FAILURE_HOOK));
        assert!(settings["hooks"].get(STOP_FAILURE_HOOK).is_none());
    }

    /// Issue #537 item 4.6: what an install reports. `hooks install` prints
    /// `installed` and `skipped` to the user, and `auto_install` skips the write
    /// when nothing was installed, so a first install must report every hook
    /// type as installed and a repeat of it every type as already there.
    ///
    /// The binary is a real executable: a pin naming a file that is not there
    /// is dead, and an install rightly rewrites a dead pin every time.
    #[cfg(unix)]
    #[test]
    fn an_install_reports_what_it_wrote_and_a_repeat_reports_it_already_there() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().expect("binary tempdir");
        let binary_path = dir.path().join(DEFAULT_BINARY_NAME);
        crate::test_isolation::write_script(&binary_path, "#!/bin/sh\nexit 0\n")
            .expect("write binary");
        std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod binary");
        let binary = binary_path.to_str().expect("utf-8 tempdir");
        let mut settings = serde_json::json!({});

        let first = install_impl(&mut settings, binary, false);
        assert_eq!(
            first.installed,
            hook_types(ClaudeHookGates::stop_failure_only(false))
        );
        assert!(first.skipped.is_empty(), "{:?}", first.skipped);

        let second = install_impl(&mut settings, binary, false);
        assert!(
            second.installed.is_empty(),
            "a repeat install reported types as newly installed: {:?}",
            second.installed
        );
        assert_eq!(
            second.skipped,
            hook_types(ClaudeHookGates::stop_failure_only(false))
        );
        assert_eq!(second.repaired, 0);
    }

    /// A settings file that exists but cannot be read is refused, not taken for
    /// an absent one: reading it as `{}` would publish a hooks-only file over
    /// the user's settings. Only a missing file starts from empty.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_settings_file_is_refused_and_left_as_it_was() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().expect("settings tempdir");
        let settings = dir.path().join("settings.json");
        let original = br#"{"model":"opus"}"#;
        std::fs::write(&settings, original).expect("seed settings");
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o200))
            .expect("make settings write-only");
        if std::fs::read(&settings).is_ok() {
            eprintln!("SKIP: this user reads a 0200 file (root), so it cannot be unreadable");
            return;
        }

        let result = install_to(&settings, "/opt/deck/dot-agent-deck");
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600))
            .expect("restore settings mode");

        let err = result.expect_err("an unreadable settings file must refuse the install");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{err}");
        assert_eq!(
            std::fs::read(&settings).expect("read settings"),
            original,
            "the refused install replaced the user's settings"
        );
    }

    /// Write `body` as an executable stand-in `claude` in `dir`.
    #[cfg(unix)]
    fn stand_in_claude(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("claude");
        crate::test_isolation::write_script(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Issue #714 (audit A2): a stand-in `claude` that exits at once but leaves
    /// a background child holding its stdout open, having printed no complete
    /// line. The probe answers unknown within its bound instead of waiting for
    /// EOF, and the helper holding the pipe is killed with the probe's group.
    #[cfg(unix)]
    #[test]
    fn the_version_probe_is_bounded_when_a_descendant_holds_stdout_open() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let claude = stand_in_claude(
            dir.path(),
            &format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nprintf '2.1.300'\nexit 0\n",
                pid_file.display()
            ),
        );
        // Leave room for the stand-in to be scheduled when the fast tier is
        // running in parallel; otherwise the probe can time out before the
        // script even writes helper.pid, leaving this test without its premise.
        let bound = std::time::Duration::from_secs(3);
        let started = std::time::Instant::now();
        let outcome = probe_claude_version(claude.as_os_str(), bound);
        let elapsed = started.elapsed();
        assert_eq!(outcome, (false, None), "an unfinished line is unknown");
        assert!(
            elapsed < bound + std::time::Duration::from_millis(1500),
            "the probe waited {elapsed:?} against a {bound:?} bound"
        );

        let helper: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z'))
            })
        };
        if Path::new("/proc/self/stat").exists() {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !gone(helper) && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(gone(helper), "the helper holding stdout outlived the probe");
        }
    }

    /// Issue #1454: a stand-in `claude` whose helper escapes the probe's
    /// process group (`setsid`) and keeps stdout open past the deadline. Killing
    /// the group cannot reach it, so the probe must not leave anything of its
    /// own holding the pipe's read end: once the probe returns, the escaped
    /// helper's next write to stdout fails with `EPIPE`. Before the fix a
    /// detached reader thread owned the read end and sat blocked on it for as
    /// long as the helper lived, so that write succeeded — the read end being
    /// closed is the observable that no reader outlived the probe.
    #[cfg(unix)]
    #[test]
    fn the_version_probe_leaves_no_reader_behind_when_a_descendant_escapes_its_group() {
        if std::process::Command::new("perl")
            .args(["-MPOSIX", "-e", "exit 0"])
            .status()
            .map_or(true, |status| !status.success())
        {
            eprintln!("SKIP: perl with POSIX is needed to escape the probe's process group");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("escapee.pid");
        let go_file = dir.path().join("go");
        let result_file = dir.path().join("result");
        // The escapee: a new session (so a new process group), SIGPIPE ignored
        // so a closed read end reads as a failed write rather than a death,
        // then wait for the test's go-ahead, try to write, record whether the
        // write went through, and exit. Without a go-ahead it gives up after
        // 20s, so a failing run does not leave it behind for long.
        let escapee = r#"use POSIX; POSIX::setsid() or die "setsid: $!"; $SIG{PIPE} = "IGNORE"; my (undef, $go, $result) = @ARGV; for (1 .. 400) { last if -e $go; select(undef, undef, undef, 0.05) } my $wrote = syswrite(STDOUT, "x"); open(my $f, ">", "$result.tmp") or die; print $f (defined $wrote ? "open" : "closed"); close $f; rename("$result.tmp", $result); exit 0"#;
        let claude = stand_in_claude(
            dir.path(),
            &format!(
                "#!/bin/sh\nperl -e '{escapee}' '{pid}' '{go}' '{result}' &\necho $! > '{pid}.tmp' && mv '{pid}.tmp' '{pid}'\nsleep 0.5\nprintf '2.1.300'\nexit 0\n",
                pid = pid_file.display(),
                go = go_file.display(),
                result = result_file.display(),
            ),
        );
        let bound = std::time::Duration::from_secs(3);
        let started = std::time::Instant::now();
        let outcome = probe_claude_version(claude.as_os_str(), bound);
        let elapsed = started.elapsed();
        assert_eq!(outcome, (false, None), "an unfinished line is unknown");
        assert!(
            elapsed < bound + std::time::Duration::from_millis(1500),
            "the probe waited {elapsed:?} against a {bound:?} bound"
        );

        // The escapee is never signalled: it exits on its own once it has
        // written its result, or after its 20s cap, so there is no pid to
        // outlive it and be reused (Greptile on PR #1512).
        assert!(
            pid_file.exists(),
            "the stand-in started its helper before the deadline"
        );
        std::fs::write(&go_file, b"").unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !result_file.exists() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let result = std::fs::read_to_string(&result_file).expect(
            "the escaped helper survived the probe's group kill and wrote its result \
             (if it did not, it was killed with the group and this test lost its premise)",
        );
        assert_eq!(
            result, "closed",
            "the pipe's read end outlived the probe: something the probe started is still reading it"
        );
    }

    /// Issue #714: a stand-in `claude` that prints a full, accepted version
    /// line and then hangs in the foreground. The line alone is not enough: the
    /// probe kills the child at its bound and answers unknown, so a wrapper
    /// that stalls never gets `StopFailure` installed.
    #[cfg(unix)]
    #[test]
    fn the_version_probe_answers_unknown_when_the_child_hangs_after_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let claude = stand_in_claude(
            dir.path(),
            "#!/bin/sh\necho '2.1.300 (Claude Code)'\nsleep 30\n",
        );
        let bound = std::time::Duration::from_millis(500);
        let started = std::time::Instant::now();
        let outcome = probe_claude_version(claude.as_os_str(), bound);
        let elapsed = started.elapsed();
        assert_eq!(
            outcome,
            (false, None),
            "a line from a child killed at the deadline is unknown"
        );
        assert!(
            elapsed < bound + std::time::Duration::from_millis(1500),
            "the probe waited {elapsed:?} against a {bound:?} bound"
        );
    }

    /// Issue #714 (audit A2): the probe needs the version LINE, not EOF — a
    /// stand-in that prints a full line and leaves a helper holding stdout is
    /// read as soon as the line arrives.
    #[cfg(unix)]
    #[test]
    fn the_version_probe_reads_the_line_without_waiting_for_eof() {
        let dir = tempfile::tempdir().unwrap();
        let claude = stand_in_claude(
            dir.path(),
            "#!/bin/sh\nsleep 3 &\necho '2.1.300 (Claude Code)'\nexit 0\n",
        );
        let started = std::time::Instant::now();
        let outcome = probe_claude_version(claude.as_os_str(), std::time::Duration::from_secs(10));
        assert_eq!(outcome, (true, Some("2.1.300 (Claude Code)".to_string())));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the probe waited {:?} for a pipe it did not need",
            started.elapsed()
        );
        assert_eq!(
            probe_claude_version(
                dir.path().join("missing").as_os_str(),
                std::time::Duration::from_secs(1)
            ),
            (false, None)
        );
    }

    /// PRD #1666 review SF-2: the waiter is found on SessionStart by its
    /// command suffix, whatever executable spelling precedes it.
    #[test]
    fn settings_have_waiter_reads_only_a_session_start_waiter() {
        let with = |event: &str, command: &str| {
            json!({"hooks":{event:[{"hooks":[{"type":"command","command":command}]}]}}).to_string()
        };
        assert!(settings_have_waiter(&with(
            "SessionStart",
            "'/opt/my deck/dot-agent-deck' inbox wait --agent claude-code"
        )));
        assert!(!settings_have_waiter(&with(
            "SessionStart",
            "/opt/dot-agent-deck hook --agent claude-code"
        )));
        assert!(!settings_have_waiter(&with(
            "Stop",
            "/opt/dot-agent-deck inbox wait --agent claude-code"
        )));
        assert!(!settings_have_waiter("not json"));
        assert!(!settings_have_waiter("{}"));
    }
}
