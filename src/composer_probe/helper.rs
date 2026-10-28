//! PRD #1666 part 3 (Q35-12; core §5.2–§5.3, R4 §2.3–§2.4 with R5 §2.4 and
//! §7.3, R3 §4.5–§4.6 with R4 §6.3, amendments 3–6): the editor helper. An
//! installed agent runs the daemon's private copy of the deck binary
//! ([`super::instance`]) as its editor, under the name [`HELPER_NAME`], and
//! the binary dispatches on that basename before it parses a command line
//! ([`run_if_invoked`]). Linux only, like the instance it lives in.
//!
//! **What it decides** ([`branch`]). It reads its generation's arm page and
//! classifies its parent `Q` ([`super::classifier`]), holding pidfds for
//! itself and `Q` from before the first read:
//! - a tool's invocation ([`Class::PositivelyTool`]) runs the tool's editor,
//!   after reporting `tool` on a marker and the socket;
//! - with the arm present, it opens **no** editor: it reports `armed`, and
//!   the daemon's answer, or a failed exchange, decides its exit and whether
//!   it prints the one fixed notice ([`armed_exit`]), under a 5 s alarm whose
//!   default action ends it;
//! - with the arm absent, the agent's own invocation reports `unarmed` and
//!   runs the user's editor with the agent's selection ([`editor_command`]);
//!   any other runs the user's editor without a report when its argument has
//!   the agent's export shape ([`export_shaped`]), and the tool's editor
//!   otherwise.
//!
//! **The editor** runs as a child, with the signal mask and the SIGINT,
//! SIGQUIT, SIGALRM and SIGCHLD dispositions it inherited, and with
//! `VISUAL`/`EDITOR` restored and the helper's own variables removed
//! ([`editor_env`]). SIGPIPE is the exception: the Rust runtime ignored it
//! before `main`, so what the agent gave is unknown, and the editor gets the
//! default, as `std::process::Command` gives its children. While it runs, the
//! helper ignores SIGINT and SIGQUIT, is its descendants' subreaper, and has
//! no alarm. It exits with the editor's status, or re-raises the editor's
//! signal with that signal's default action, unblocked.
//!
//! **After the editor** (amendment 4 §7.5, amendment 6 §2.3), from `t0`, the
//! moment the editor phase ended (its reap, a failed start, or a failed
//! wait), every step uses the time left to `D = t0 + 1 s`, and a step with
//! no time left is skipped rather than called: the reap loop over the
//! editor's descendants, the report on the connection opened before the
//! editor, and the wait for its acknowledgement. The socket is nonblocking,
//! so `EINTR` and partial I/O are charged against `D`. Nothing on this path
//! writes to a terminal; the armed path's notice is the helper's only
//! terminal write. So the helper exits about 1 s after `t0` at most, plus the
//! kernel's time in its last nonblocking calls, as long as it is scheduled
//! and not stopped: a stopped helper runs no step at all until it is
//! continued.
//!
//! **What this is not, yet.** The daemon advertises neither capability and
//! refuses every `composer_invocation` as unsupported until Q35-14b, so with
//! this build's daemon every exchange fails and no report is sent; installation's rollout is empty (Q35-13), so no
//! production agent runs the helper. Its premises about the kernel's reaper,
//! wait and tty write paths (P-SR′, P-DRN, N1) are UNVERIFIED pending E-PIN
//! (amendment 5 §3.1): until that evidence is in `docs/develop/`, a `Clean`
//! report is the helper's observation, not an attestation. Nothing reads a
//! report yet, and under the default policy amendment 5 sets (`Hold`, Q35-07)
//! a `Clean` one authorises nothing.
//!
//! **Test seams** ([`seam`]) exist only in the `e2e` build: a pause point,
//! injected `posix_spawn` and `waitpid` failures, a trace of the post-editor
//! syscalls, a small send buffer with a padded frame, an `EINTR` source and
//! the A5 mutant. A build without `e2e` compiles none of them.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::classifier::{self, Class};
use super::install::{self, ProbeAgent, SavedEditor};
use super::instance::{self, DaemonId, HELPER_NAME, InstanceReader, Kind};
use crate::event::{
    ComposerClaim, ComposerDecision, ComposerInvocationReply, ComposerInvocationRequest,
    ComposerOutcome, DaemonMessage, EDITOR_FRONTIER, EditorReport, EditorTree, ExportRead,
    HookCapabilitiesReply, HookCapabilitiesRequest,
};

/// The armed path's notice (R4 §2.4), its whole terminal output.
pub const NOTICE: &[u8] = b"dot-agent-deck: the deck was checking this input box. \
If you pressed Ctrl+G, press it again in a moment.\n";

/// The armed path's alarm, armed as the branch is taken (core §5.2).
pub const ARMED_ALARM: Duration = Duration::from_secs(5);

/// The bound on one exchange with the daemon: connect, the capability
/// question and the invocation with their answers.
pub const EXCHANGE_BOUND: Duration = Duration::from_secs(2);

/// How long the notice stays before the helper exits.
pub const NOTICE_WAIT: Duration = Duration::from_millis(1500);

/// `D - t0`: the whole post-editor path.
pub const POST_EDITOR: Duration = Duration::from_secs(1);

/// The reap loop's share of the post-editor path.
pub const REAP_BUDGET: Duration = Duration::from_millis(100);

/// The most descendants the reap loop reaps before it settles as unknown.
pub const REAP_MAX: usize = 256;

/// The exit status when the editor couldn't be started.
pub const SPAWN_FAILED_STATUS: i32 = 127;

/// The exit status when no editor is configured, or the editor's wait failed.
pub const FAILED_STATUS: i32 = 1;

/// The longest line the helper reads from the daemon.
const MAX_LINE: usize = 64 << 10;

/// Claude Code's default editors, in the order it looks for them (M22).
pub const CLAUDE_DEFAULTS: &[&str] = &["code", "vi", "nano"];

/// The editor a learned generation falls back to (R5 §10.3).
pub const LEARNED_DEFAULT: &str = "vi";

// ---------------------------------------------------------------------------
// The pure parts: selection, shape, branch, exit, marker and environment.
// ---------------------------------------------------------------------------

/// `value` split on literal spaces, as Claude Code splits `VISUAL` (M23),
/// with empty words dropped. Nothing is evaluated.
pub fn split_spaces(value: &OsStr) -> Vec<OsString> {
    value
        .as_bytes()
        .split(|b| *b == b' ')
        .filter(|word| !word.is_empty())
        .map(|word| OsString::from_vec(word.to_vec()))
        .collect()
}

/// `value` split by shell-words rules, as Codex splits `VISUAL` (M34):
/// blanks separate words, single quotes keep everything literally, double
/// quotes keep everything but a backslash before `\`, `"`, `$`, `` ` `` or a
/// newline, and a backslash outside quotes keeps the next byte. Nothing is
/// evaluated: `;`, `$(…)`, `|` and redirections are bytes in a word. `None`
/// for an unterminated quote or a trailing backslash.
pub fn split_shell_words(value: &OsStr) -> Option<Vec<OsString>> {
    #[derive(PartialEq)]
    enum In {
        Blank,
        Word,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut state = In::Blank;
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(b) = bytes.next() {
        match state {
            In::Blank | In::Word => match b {
                b' ' | b'\t' | b'\n' => {
                    if state == In::Word {
                        words.push(OsString::from_vec(std::mem::take(&mut word)));
                    }
                    state = In::Blank;
                }
                b'\'' => state = In::Single,
                b'"' => state = In::Double,
                b'\\' => {
                    word.push(bytes.next()?);
                    state = In::Word;
                }
                _ => {
                    word.push(b);
                    state = In::Word;
                }
            },
            In::Single => match b {
                b'\'' => state = In::Word,
                _ => word.push(b),
            },
            In::Double => match b {
                b'"' => state = In::Word,
                b'\\' => {
                    let next = bytes.next()?;
                    if !matches!(next, b'\\' | b'"' | b'$' | b'`' | b'\n') {
                        word.push(b'\\');
                    }
                    word.push(next);
                }
                _ => word.push(b),
            },
        }
    }
    match state {
        In::Single | In::Double => None,
        In::Word => {
            words.push(OsString::from_vec(word));
            Some(words)
        }
        In::Blank => Some(words),
    }
}

/// Which editor an invocation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The user's editor, chosen as the agent chooses it (R3 §4.6, R4
    /// §6.3): the agent's own invocation, or one with its export shape.
    User,
    /// A tool's editor (R3 §4.5): the restored `VISUAL`, else `EDITOR`.
    Tool,
}

/// Why no editor command could be chosen. The helper exits
/// [`FAILED_STATUS`] and writes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoEditor {
    /// Codex's selection: `VISUAL` set and empty.
    EmptyVisual,
    /// Codex's selection: `VISUAL` unset and `EDITOR` set and empty.
    EmptyEditor,
    /// Neither is set, and the agent has no default to fall back to.
    Unset,
    /// The chosen value has no word, or an unterminated quote.
    Unsplittable,
}

/// Whether `value`'s first word names the helper itself: the recursion
/// guard (R2 §7.2), which takes such a value as unset.
fn names_helper(value: &OsStr) -> bool {
    split_spaces(value).first().is_some_and(|word| {
        Path::new(word)
            .file_name()
            .is_some_and(|name| name == OsStr::new(HELPER_NAME))
    })
}

/// A saved value, unless the recursion guard drops it.
fn guarded(value: &Option<OsString>) -> Option<&OsStr> {
    value.as_deref().filter(|v| !names_helper(v))
}

/// A saved value that is set and not empty, after the guard.
fn truthy(value: &Option<OsString>) -> Option<&OsStr> {
    guarded(value).filter(|v| !v.is_empty())
}

/// The editor command for `kind` on `route`, from the user's saved
/// `VISUAL`/`EDITOR` (the helper's own value in either is taken as unset).
/// `on_path` says whether a default editor is installed. The agent's own
/// arguments go after these words.
///
/// - Claude Code: a truthy `VISUAL`, else a truthy `EDITOR`, else the first
///   of [`CLAUDE_DEFAULTS`] on `PATH`, split on spaces; on the user's route,
///   exactly `code` becomes `code -w` and exactly `subl` `subl --wait` (M23).
///   The line-number arguments Claude gives a known editor family by name
///   aren't reproduced: the invocation the helper receives doesn't carry the
///   line.
/// - Codex, user's route: a set `VISUAL`, which mustn't be empty, else a set
///   `EDITOR`, likewise, else nothing (M34). Tool route: a non-empty `VISUAL`,
///   else a non-empty `EDITOR`, else nothing. Shell words either way.
/// - Learned (R5 §10.3): a non-empty `VISUAL`, else a non-empty `EDITOR`,
///   else [`LEARNED_DEFAULT`], in shell words.
pub fn editor_command(
    kind: Kind,
    route: Route,
    saved: &SavedEditor,
    on_path: impl Fn(&OsStr) -> bool,
) -> Result<Vec<OsString>, NoEditor> {
    match kind {
        Kind::Agent(ProbeAgent::Claude) => {
            let value = truthy(&saved.visual)
                .or_else(|| truthy(&saved.editor))
                .map(OsStr::to_os_string)
                .or_else(|| {
                    CLAUDE_DEFAULTS
                        .iter()
                        .map(OsStr::new)
                        .find(|name| on_path(name))
                        .map(OsStr::to_os_string)
                })
                .ok_or(NoEditor::Unset)?;
            let mut words = split_spaces(&value);
            if route == Route::User {
                if value == "code" {
                    words.push("-w".into());
                } else if value == "subl" {
                    words.push("--wait".into());
                }
            }
            if words.is_empty() {
                return Err(NoEditor::Unsplittable);
            }
            Ok(words)
        }
        Kind::Agent(ProbeAgent::Codex) => {
            let value = match route {
                Route::User => match (guarded(&saved.visual), guarded(&saved.editor)) {
                    (Some(v), _) if v.is_empty() => return Err(NoEditor::EmptyVisual),
                    (Some(v), _) => v,
                    (None, Some(e)) if e.is_empty() => return Err(NoEditor::EmptyEditor),
                    (None, Some(e)) => e,
                    (None, None) => return Err(NoEditor::Unset),
                },
                Route::Tool => truthy(&saved.visual)
                    .or_else(|| truthy(&saved.editor))
                    .ok_or(NoEditor::Unset)?,
            };
            shell_command(value)
        }
        Kind::Learned => shell_command(
            truthy(&saved.visual)
                .or_else(|| truthy(&saved.editor))
                .unwrap_or(OsStr::new(LEARNED_DEFAULT)),
        ),
    }
}

fn shell_command(value: &OsStr) -> Result<Vec<OsString>, NoEditor> {
    match split_shell_words(value) {
        Some(words) if !words.is_empty() => Ok(words),
        _ => Err(NoEditor::Unsplittable),
    }
}

/// Whether `path` is under `dir`, compared component by component, with no
/// `..` and no `.` on the way.
fn lexically_under(path: &Path, dir: &Path) -> bool {
    // Read as bytes: `Path::components` drops a `.` it meets.
    let plain = |p: &Path| {
        p.as_os_str()
            .as_bytes()
            .split(|b| *b == b'/')
            .all(|part| part != b"." && part != b"..")
    };
    path.is_absolute() && plain(path) && plain(dir) && path.starts_with(dir) && path != dir
}

/// Whether the last of `args` has `kind`'s export shape (core §2.3): Claude
/// Code's, a file under `<TMPDIR>/claude-<uid>/` (`/tmp` when `TMPDIR` is
/// unset or empty); Codex's, `<CODEX_HOME>/editor/.tmp*.md` (`~/.codex` when
/// `CODEX_HOME` is unset or empty). Compared as text: this chooses between
/// the user's editor and a tool's, and authorises nothing. A learned
/// generation has no shape.
pub fn export_shaped(
    kind: Kind,
    args: &[OsString],
    env: impl Fn(&str) -> Option<OsString>,
    uid: u32,
) -> bool {
    let Some(last) = args.last().map(Path::new) else {
        return false;
    };
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    match kind {
        Kind::Agent(ProbeAgent::Claude) => {
            let tmp = set("TMPDIR").unwrap_or_else(|| PathBuf::from("/tmp"));
            lexically_under(last, &tmp.join(format!("claude-{uid}")))
        }
        Kind::Agent(ProbeAgent::Codex) => {
            let Some(home) = set("CODEX_HOME").or_else(|| set("HOME").map(|h| h.join(".codex")))
            else {
                return false;
            };
            let name = last.file_name().map(OsStr::as_bytes).unwrap_or_default();
            lexically_under(last, &home.join("editor"))
                && last.parent() == Some(&home.join("editor"))
                && name.starts_with(b".tmp")
                && name.ends_with(b".md")
        }
        Kind::Learned => false,
    }
}

/// What the arm page said (R5 §2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    /// No page, or a consistent read with a count of 0.
    Absent { seen: Option<Seen> },
    /// A count above 0, or no consistent read (an unreadable page reads as
    /// armed).
    Present { seen: Option<Seen> },
}

impl Arm {
    /// The segment and native numbers read with the count, when the read was
    /// consistent.
    pub fn seen(&self) -> Option<Seen> {
        match self {
            Arm::Absent { seen } | Arm::Present { seen } => *seen,
        }
    }
}

/// `seg_seq` and `native_seq` from the publication the count came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub seg: u64,
    pub native: u64,
}

/// What the helper does (core §5.2, amendment 5 §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    /// A tool's invocation: report `tool`, then the tool's editor.
    Tool,
    /// The arm is present: report `armed`; no editor.
    Armed,
    /// The agent's own invocation with no arm: report `unarmed`, then the
    /// user's editor with the agent's selection.
    Unarmed,
    /// Not the agent's for certain, no arm, and the export shape: the user's
    /// editor, with no report on any channel.
    UnreportedUser,
    /// The same without the export shape, or no instance to report to: the
    /// tool's editor, with no report.
    UnreportedTool,
}

/// The branch for an arm read and a class (R5 §2.4): a tool is a tool
/// whatever the arm, an arm present opens no editor, and only the agent's
/// own invocation reports an absent one.
pub fn branch(arm: Arm, class: Class, shaped: bool) -> Branch {
    match (arm, class) {
        (_, Class::PositivelyTool) => Branch::Tool,
        (Arm::Present { .. }, _) => Branch::Armed,
        (Arm::Absent { .. }, Class::Owned) => Branch::Unarmed,
        (Arm::Absent { .. }, Class::Unknown) if shaped => Branch::UnreportedUser,
        (Arm::Absent { .. }, Class::Unknown) => Branch::UnreportedTool,
    }
}

/// How the armed path ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmedExit {
    pub notice: bool,
    pub status: i32,
}

/// The return table (R4 §2.4, R5 §7.3) for `kind` and the daemon's
/// `outcome`, `None` being row T, a failed exchange: every armed invocation
/// leaves the composer as it was, and exits 0 on Claude Code, or on Codex
/// for an attested empty read; otherwise 1, Codex's failed-editor path. A
/// learned generation exits as Codex does. An `Unarmed` answer to an armed
/// invocation opens no editor here: it is read as `Uncertain`.
pub fn armed_exit(kind: Kind, outcome: Option<ComposerOutcome>) -> ArmedExit {
    let claude = kind == Kind::Agent(ProbeAgent::Claude);
    let read_status = |read: ExportRead| i32::from(!(claude || read == ExportRead::Empty));
    match outcome {
        Some(ComposerOutcome::Mine { read }) => ArmedExit {
            notice: false,
            status: read_status(read),
        },
        Some(ComposerOutcome::MineWithNotice { read }) => ArmedExit {
            notice: true,
            status: read_status(read),
        },
        Some(ComposerOutcome::Uncertain | ComposerOutcome::Unarmed | ComposerOutcome::Unknown)
        | None => ArmedExit {
            notice: true,
            status: i32::from(!claude),
        },
    }
}

/// The marker's name for an invocation (core §2.3, amendment 2 §4.4):
/// `<pid>-<start>-<decision>`, then `-<seg>-<native>` when the arm read was
/// consistent. Its grammar, with the daemon's `.seen`:
/// `^[0-9]{1,10}-[0-9]{1,20}-(armed|unarmed|tool)(-[0-9]{1,20}-[0-9]{1,20})?(\.seen)?$`.
pub fn marker_name(pid: u32, start: u64, decision: ComposerDecision, seen: Option<Seen>) -> String {
    let word = match decision {
        ComposerDecision::Armed => "armed",
        ComposerDecision::Unarmed => "unarmed",
        ComposerDecision::Tool | ComposerDecision::Unknown => "tool",
    };
    match seen {
        Some(seen) => format!("{pid}-{start}-{word}-{}-{}", seen.seg, seen.native),
        None => format!("{pid}-{start}-{word}"),
    }
}

/// The editor's environment from the helper's: every variable under the
/// helper's reserved prefix removed, and `VISUAL`/`EDITOR` as the user had
/// them, set or unset (R3 §4.5 step 1).
pub fn editor_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
    saved: &SavedEditor,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = vars
        .into_iter()
        .filter(|(name, _)| name != "VISUAL" && name != "EDITOR" && !install::helper_reserved(name))
        .collect();
    for (name, value) in [("VISUAL", &saved.visual), ("EDITOR", &saved.editor)] {
        if let Some(value) = value {
            env.push((name.into(), value.clone()));
        }
    }
    env
}

/// A point in time the post-editor path counts down to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(start: Instant, bound: Duration) -> Deadline {
        Deadline(start + bound)
    }

    /// The time left, or `None` once there is none: the step is skipped.
    pub fn remaining(&self) -> Option<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }

    /// The time left as a `poll` timeout, rounded up to a whole millisecond
    /// so that a poll never returns before the deadline it was given.
    fn poll_ms(&self) -> Option<libc::c_int> {
        let left = self.remaining()?;
        let ms = left.as_micros().div_ceil(1000);
        Some(libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX))
    }
}

// ---------------------------------------------------------------------------
// The run.
// ---------------------------------------------------------------------------

/// Run the helper and exit, when this process was invoked as
/// [`HELPER_NAME`]; return otherwise. Called first in `main`, so the helper
/// runs single-threaded, before anything parses the command line.
pub fn run_if_invoked() {
    let mut args = std::env::args_os();
    let invoked = args
        .next()
        .is_some_and(|argv0| Path::new(&argv0).file_name() == Some(OsStr::new(HELPER_NAME)));
    if invoked {
        finish(run(args.collect()));
    }
}

/// How the helper ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Status(i32),
    Signal(i32),
}

fn finish(end: End) -> ! {
    match end {
        End::Status(code) => std::process::exit(code),
        End::Signal(sig) => {
            // SAFETY: plain signal calls on this process: its default action,
            // unblocked, then raised.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                let mut set = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, sig);
                libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                libc::raise(sig);
            }
            // A signal whose default doesn't end the process can't have ended
            // the editor either; this is the shell's convention.
            std::process::exit(128 + sig)
        }
    }
}

/// The signal state the helper inherited (core §5.2 step 0).
struct Inherited {
    mask: libc::sigset_t,
    /// `(signal, disposition)` for each signal the helper changes.
    actions: Vec<(libc::c_int, libc::sigaction)>,
}

/// The signals whose disposition the helper changes; the editor gets each
/// back as inherited.
const CHANGED: [libc::c_int; 4] = [libc::SIGINT, libc::SIGQUIT, libc::SIGALRM, libc::SIGCHLD];

impl Inherited {
    /// Save the mask and dispositions, then set SIGALRM and SIGCHLD to their
    /// defaults (SIGCHLD ignored would let the kernel reap the editor) and
    /// unblock SIGALRM.
    fn take() -> Inherited {
        // SAFETY: queries and changes of this process's own signal state,
        // into and from zeroed values of the right types.
        unsafe {
            let mut mask = std::mem::zeroed::<libc::sigset_t>();
            libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
            let actions = CHANGED
                .iter()
                .map(|&sig| {
                    let mut old = std::mem::zeroed::<libc::sigaction>();
                    libc::sigaction(sig, std::ptr::null(), &mut old);
                    (sig, old)
                })
                .collect();
            libc::signal(libc::SIGALRM, libc::SIG_DFL);
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            let mut alarm = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut alarm);
            libc::sigaddset(&mut alarm, libc::SIGALRM);
            libc::sigprocmask(libc::SIG_UNBLOCK, &alarm, std::ptr::null_mut());
            Inherited { mask, actions }
        }
    }
}

fn set_alarm(after: Duration) {
    let timer = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: libc::timeval {
            tv_sec: after.as_secs() as libc::time_t,
            tv_usec: after.subsec_micros() as libc::suseconds_t,
        },
    };
    // SAFETY: a valid timer value; the old one isn't asked for.
    unsafe { libc::setitimer(libc::ITIMER_REAL, &timer, std::ptr::null_mut()) };
}

/// A pidfd, or `None` where the kernel or the process refuses one.
fn pidfd(pid: u32) -> Option<OwnedFd> {
    // SAFETY: `pidfd_open(pid, 0)`; a non-negative result is a new
    // descriptor, close-on-exec, owned here.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Whether the process behind `pidfd` has exited (its pidfd reads ready).
fn exited(pidfd: &OwnedFd) -> bool {
    let mut fds = [libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    // SAFETY: one valid pollfd, no wait.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
    rc != 0
}

/// The instance this helper copy belongs to: the directory its executable
/// is in, readable as one, with the daemon record its name names.
fn find_instance() -> Option<(InstanceReader, DaemonId)> {
    let exe = std::fs::read_link("/proc/self/exe").ok()?;
    let dir = exe.parent()?;
    let (pid, start) = instance::parse_instance_name(dir.file_name()?.to_str()?)?;
    let reader = InstanceReader::open(dir).ok()?;
    let daemon = reader
        .daemon()
        .filter(|d| (d.pid, d.start) == (pid, start))?;
    Some((reader, daemon))
}

/// Read `arm-G` once (R5 §2.4): no file is absent; one that can't be
/// mapped, is shorter than the words or reads inconsistently is present.
fn read_arm(reader: &InstanceReader, generation: &str) -> Arm {
    let present = Arm::Present { seen: None };
    let file = match reader.arm(generation) {
        Ok(None) => return Arm::Absent { seen: None },
        Ok(Some(file)) => file,
        Err(_) => return present,
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return present;
    };
    let len = usize::try_from(len)
        .unwrap_or(usize::MAX)
        .min(instance::PAGE_BYTES);
    if len < super::arm::WORDS_BYTES {
        return present;
    }
    // SAFETY: a read-only shared mapping of `len` bytes of an open regular
    // file at least that long; the result is checked before use.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return present;
    }
    // SAFETY: the mapping just made, `len` bytes, page-aligned; only the
    // daemon writes the words, atomically (`super::arm`).
    let read = unsafe { super::arm::ArmReader::at(ptr.cast(), len) }
        .map_err(super::arm::Unread::Page)
        .and_then(|reader| reader.read_publication());
    // SAFETY: the mapping made above, unmapped once.
    unsafe { libc::munmap(ptr, len) };
    match read {
        Ok(p) => {
            let seen = Some(Seen {
                seg: p.seg_seq,
                native: p.native_seq,
            });
            if p.count == 0 {
                Arm::Absent { seen }
            } else {
                Arm::Present { seen }
            }
        }
        Err(_) => present,
    }
}

/// What the helper knows about its invocation, once step 1 is done.
struct Invocation {
    args: Vec<OsString>,
    generation: Option<String>,
    instance: Option<(InstanceReader, DaemonId)>,
    kind: Kind,
    saved: SavedEditor,
    pid: u32,
    start: u64,
}

fn run(args: Vec<OsString>) -> End {
    let inherited = Inherited::take();
    let seams = seam::Seams::read();
    seams.trace("start");
    seams.pause(seam::Point::Start);
    // The pins come first (R35CODE4-U3-01): pidfds for this process and its
    // parent, before anything about either is read, held to the exit.
    let pid = std::process::id();
    let _own = pidfd(pid);
    let q = crate::platform::proc::current_ppid() as u32;
    let q_pin = pidfd(q);
    let q_start = crate::platform::proc::process_start_of(q);
    let start = crate::platform::proc::process_start_of(pid).unwrap_or(0);

    let generation = std::env::var(install::GENERATION_ENV)
        .ok()
        .filter(|g| instance::generation_name_ok(g));
    let instance = generation.as_ref().and_then(|_| find_instance());
    let kind = match (&instance, &generation) {
        (Some((reader, _)), Some(g)) => reader.kind(g).unwrap_or(Kind::Learned),
        _ => Kind::Learned,
    };
    let invocation = Invocation {
        args,
        saved: install::saved_editor(|name| std::env::var_os(name)),
        generation,
        instance,
        kind,
        pid,
        start,
    };
    let (Some((reader, _)), Some(generation)) = (&invocation.instance, &invocation.generation)
    else {
        // No generation or no instance: no deck generation can have armed
        // this process (R4 §2.3 step 0).
        return editor_path(
            &invocation,
            &inherited,
            &seams,
            Branch::UnreportedTool,
            None,
        );
    };
    let arm = read_arm(reader, generation);
    let identity = reader.identity(generation);
    let mut class = classifier::classify(
        &mut crate::platform::proc::LinuxProcView,
        identity.as_ref(),
        q,
    );
    // `Q` exiting, or its pid naming another process, while pinned: the
    // class describes nothing that still holds.
    let q_held = q_pin.as_ref().is_some_and(|fd| !exited(fd))
        && q_start.is_some()
        && crate::platform::proc::process_start_of(q) == q_start;
    if !q_held {
        class = Class::Unknown;
    }
    let shaped = export_shaped(
        invocation.kind,
        &invocation.args,
        |name| std::env::var_os(name),
        // SAFETY: `getuid(2)` has no failure mode.
        unsafe { libc::getuid() },
    );
    let chosen = branch(arm, class, shaped);
    seams.trace(&format!("branch {chosen:?} class {class:?} arm {arm:?}"));
    match chosen {
        Branch::Armed => armed_path(&invocation, &seams, arm, class),
        Branch::Tool | Branch::Unarmed => {
            let decision = if chosen == Branch::Tool {
                ComposerDecision::Tool
            } else {
                ComposerDecision::Unarmed
            };
            mark(&invocation, decision, arm.seen());
            let conn = Exchange::run(&invocation, decision, class, arm.seen(), &seams)
                .ok()
                .map(|exchange| exchange.into_report_channel());
            editor_path(&invocation, &inherited, &seams, chosen, conn)
        }
        Branch::UnreportedUser | Branch::UnreportedTool => {
            editor_path(&invocation, &inherited, &seams, chosen, None)
        }
    }
}

/// Write the invocation's marker, best effort.
fn mark(invocation: &Invocation, decision: ComposerDecision, seen: Option<Seen>) -> Option<String> {
    let (Some((reader, _)), Some(generation)) = (&invocation.instance, &invocation.generation)
    else {
        return None;
    };
    let name = marker_name(invocation.pid, invocation.start, decision, seen);
    reader.create_marker(generation, &name).ok().map(|()| name)
}

fn claim(class: Class) -> ComposerClaim {
    match class {
        Class::Owned => ComposerClaim::Owned,
        Class::PositivelyTool => ComposerClaim::Tool,
        Class::Unknown => ComposerClaim::Unknown,
    }
}

/// The armed path (core §5.2): the alarm now, the marker, the exchange, then
/// the return table. It never runs an editor.
fn armed_path(invocation: &Invocation, seams: &seam::Seams, arm: Arm, class: Class) -> End {
    set_alarm(ARMED_ALARM);
    let marker = mark(invocation, ComposerDecision::Armed, arm.seen());
    let outcome = Exchange::run(
        invocation,
        ComposerDecision::Armed,
        class,
        arm.seen(),
        seams,
    )
    .ok()
    .map(|exchange| exchange.outcome);
    let end = armed_exit(invocation.kind, outcome);
    seams.trace(&format!("armed {outcome:?} {end:?}"));
    if end.notice {
        // One write(2) of the whole line, never retried: a short or
        // interrupted write drops the rest (amendment 5 §3.3).
        // SAFETY: fd 2 and a buffer of `NOTICE.len()` bytes.
        unsafe { libc::write(2, NOTICE.as_ptr().cast(), NOTICE.len()) };
        // The line stays readable for the notice wait, inside which the
        // daemon renames a marker it processed (core §2.3).
        let waited = Instant::now();
        while waited.elapsed() < NOTICE_WAIT {
            if let (Some(name), Some((reader, _)), Some(g)) =
                (&marker, &invocation.instance, &invocation.generation)
                && outcome.is_none()
                && reader.marker_seen(g, name)
            {
                seams.trace("marker seen");
            }
            std::thread::sleep(Duration::from_millis(50).min(NOTICE_WAIT));
        }
    }
    End::Status(end.status)
}

/// The exchange with the daemon (R3 §4.4, plan Q35-12): at most
/// [`EXCHANGE_BOUND`] for connecting to the peer named in `<instance>/daemon`,
/// the capability question on that connection, and the invocation with its
/// answer. Any failure, a refusal among them, is row T: never `Unarmed`.
struct Exchange {
    conn: Conn,
    outcome: ComposerOutcome,
    frontier: bool,
}

impl Exchange {
    fn run(
        invocation: &Invocation,
        decision: ComposerDecision,
        class: Class,
        seen: Option<Seen>,
        seams: &seam::Seams,
    ) -> Result<Exchange, ()> {
        let (Some((_, daemon)), Some(generation)) = (&invocation.instance, &invocation.generation)
        else {
            return Err(());
        };
        let generation: u64 = generation.parse().map_err(drop)?;
        let pane_id = std::env::var(crate::agent_pty::DOT_AGENT_DECK_PANE_ID).map_err(drop)?;
        let token = crate::hook_provenance::token_from_env();
        let deadline = Deadline::after(Instant::now(), EXCHANGE_BOUND);
        let mut conn = Conn::connect(&crate::platform::paths::socket_path(), daemon, deadline)?;
        let question = DaemonMessage::HookCapabilities(HookCapabilitiesRequest {
            pane_id: pane_id.clone(),
            token: token.clone(),
        });
        conn.send_line(
            &serde_json::to_vec(&question).map_err(drop)?,
            deadline,
            seams,
        )
        .map_err(drop)?;
        let answer = conn.read_line(deadline, seams).map_err(drop)?;
        crate::hook::confirms_composer_probe(&answer).map_err(drop)?;
        let capabilities: HookCapabilitiesReply =
            serde_json::from_str(answer.trim()).map_err(drop)?;
        let request = DaemonMessage::ComposerInvocation(ComposerInvocationRequest {
            pane_id,
            token,
            generation,
            decision,
            claim: Some(claim(class)),
            seg_seen: seen.map(|s| s.seg),
            native_seen: seen.map(|s| s.native),
            argv: invocation
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        });
        conn.send_line(
            &serde_json::to_vec(&request).map_err(drop)?,
            deadline,
            seams,
        )
        .map_err(drop)?;
        let reply: ComposerInvocationReply =
            serde_json::from_str(conn.read_line(deadline, seams).map_err(drop)?.trim())
                .map_err(drop)?;
        let outcome = reply.outcome.ok_or(())?;
        Ok(Exchange {
            conn,
            outcome,
            frontier: capabilities.editor_frontier == Some(EDITOR_FRONTIER),
        })
    }

    fn into_report_channel(self) -> ReportChannel {
        ReportChannel {
            conn: self.conn,
            frontier: self.frontier,
        }
    }
}

/// The connection an exchange succeeded on, kept through the editor for its
/// report (amendment 4 §7.5): nothing connects after the editor.
struct ReportChannel {
    conn: Conn,
    frontier: bool,
}

/// A nonblocking connection to the daemon's hook socket, whose every call is
/// bounded by a [`Deadline`] and skipped when it has passed.
struct Conn {
    stream: UnixStream,
    read: Vec<u8>,
}

impl Conn {
    /// Connect by `deadline`, and refuse a peer other than `daemon`: its
    /// pid, that pid's start ticks and this boot.
    fn connect(path: &Path, daemon: &DaemonId, deadline: Deadline) -> Result<Conn, ()> {
        let left = deadline.remaining().ok_or(())?;
        let stream = crate::platform::ipc::IpcClient::connect_timeout(path, left)
            .map_err(drop)?
            .into_std();
        let peer = crate::platform::peercred::peer_pid_raw(stream.as_raw_fd()).map_err(drop)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(drop)?;
        if peer != daemon.pid
            || crate::platform::proc::process_start_of(peer) != Some(daemon.start)
            || boot.trim() != daemon.boot
        {
            return Err(());
        }
        stream.set_nonblocking(true).map_err(drop)?;
        Ok(Conn {
            stream,
            read: Vec::new(),
        })
    }

    /// Wait for `events` on the socket within `deadline`: `Ok(true)` when
    /// ready, `Ok(false)` at the deadline. An interrupted poll is retried
    /// against the same deadline.
    fn wait(&self, events: libc::c_short, deadline: Deadline, seams: &seam::Seams) -> bool {
        loop {
            let Some(ms) = deadline.poll_ms() else {
                seams.trace("skip poll");
                return false;
            };
            let mut fds = [libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            }];
            // The trace names each call before it is made and its result
            // after, so a test sees when every call started.
            seams.trace(&format!("poll {events} {ms}"));
            // SAFETY: one valid pollfd.
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
            let errno = io::Error::last_os_error().raw_os_error();
            seams.trace(&format!("polled {rc}"));
            match rc {
                0 => return false,
                n if n > 0 => return true,
                _ if errno == Some(libc::EINTR) => continue,
                _ => return false,
            }
        }
    }

    /// Send `line` and its newline, whole, by `deadline`.
    fn send_line(
        &mut self,
        line: &[u8],
        deadline: Deadline,
        seams: &seam::Seams,
    ) -> io::Result<()> {
        let mut bytes = line.to_vec();
        bytes.push(b'\n');
        let mut sent = 0;
        while sent < bytes.len() {
            if !self.wait(libc::POLLOUT, deadline, seams) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            seams.trace(&format!("send {}", bytes.len() - sent));
            // SAFETY: a valid socket and the unsent rest of `bytes`; no
            // SIGPIPE on a closed peer.
            let n = unsafe {
                libc::send(
                    self.stream.as_raw_fd(),
                    bytes[sent..].as_ptr().cast(),
                    bytes.len() - sent,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            let errno = io::Error::last_os_error();
            seams.trace(&format!("sent {n}"));
            match n {
                n if n > 0 => sent += n as usize,
                _ if matches!(errno.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) => {}
                0 => return Err(io::ErrorKind::WriteZero.into()),
                _ => return Err(errno),
            }
        }
        Ok(())
    }

    /// One line, at most [`MAX_LINE`] bytes, by `deadline`.
    fn read_line(&mut self, deadline: Deadline, seams: &seam::Seams) -> io::Result<String> {
        loop {
            if let Some(end) = self.read.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.read.drain(..=end).collect();
                return String::from_utf8(line).map_err(|_| io::ErrorKind::InvalidData.into());
            }
            if self.read.len() > MAX_LINE {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if !self.wait(libc::POLLIN, deadline, seams) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut chunk = [0u8; 4096];
            seams.trace("recv");
            // SAFETY: a valid socket and a buffer of `chunk.len()` bytes.
            let n = unsafe {
                libc::recv(
                    self.stream.as_raw_fd(),
                    chunk.as_mut_ptr().cast(),
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let errno = io::Error::last_os_error();
            seams.trace(&format!("received {n}"));
            match n {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n if n > 0 => self.read.extend_from_slice(&chunk[..n as usize]),
                _ if matches!(errno.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) => {}
                _ => return Err(errno),
            }
        }
    }
}

/// How the editor phase ended.
enum Phase {
    /// No editor ran: none was configured (`None`), or starting it failed.
    SpawnFailed(Option<i32>),
    /// The editor was started and its wait failed with this errno.
    WaitUnknown,
    /// The editor was reaped with this wait status.
    Reaped(libc::c_int),
}

/// The editor paths (core §5.2–§5.3, amendment 3 §3.2, amendment 4 §7.5,
/// amendment 6 §2.3).
fn editor_path(
    invocation: &Invocation,
    inherited: &Inherited,
    seams: &seam::Seams,
    branch: Branch,
    mut channel: Option<ReportChannel>,
) -> End {
    let route = match branch {
        Branch::Unarmed | Branch::UnreportedUser => Route::User,
        _ => Route::Tool,
    };
    let env = editor_env(std::env::vars_os(), &invocation.saved);
    let path_var = env
        .iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| value.clone());
    let on_path = |name: &OsStr| resolve(name, path_var.as_deref()).is_ok();
    // SAFETY: `prctl(PR_SET_CHILD_SUBREAPER, 1)` on this process.
    let subreaper = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0;
    set_alarm(Duration::ZERO);
    ignore_terminal_signals();
    let phase = match editor_command(invocation.kind, route, &invocation.saved, on_path) {
        Err(_) => Phase::SpawnFailed(None),
        Ok(mut argv) => {
            argv.extend(invocation.args.iter().cloned());
            match spawn(&argv, &env, path_var.as_deref(), inherited, seams) {
                Err(errno) => Phase::SpawnFailed(Some(errno)),
                Ok(child) => match wait_editor(child, seams) {
                    Ok(status) => Phase::Reaped(status),
                    Err(_) => Phase::WaitUnknown,
                },
            }
        }
    };
    let t0 = Instant::now();
    let deadline = Deadline::after(t0, POST_EDITOR);
    seams.trace(&format!("t0 {}", seam::mono_ns_at(t0)));
    seams.pause(seam::Point::PostEditor);
    let (report, end) = match phase {
        Phase::SpawnFailed(errno) => {
            seams.mutant_a5(deadline);
            seams.trace(&format!("spawn-failed {errno:?}"));
            let status = if errno.is_some() {
                SPAWN_FAILED_STATUS
            } else {
                FAILED_STATUS
            };
            (
                Some(EditorReport::SpawnFailed { errno }),
                End::Status(status),
            )
        }
        // No report: the daemon keeps observing the editor itself.
        Phase::WaitUnknown => (None, End::Status(FAILED_STATUS)),
        Phase::Reaped(status) => {
            let tree = if subreaper {
                reap_tree(deadline)
            } else {
                EditorTree::Unknown
            };
            seams.trace(&format!("tree {tree:?}"));
            let end = if libc::WIFSIGNALED(status) {
                End::Signal(libc::WTERMSIG(status))
            } else {
                End::Status(libc::WEXITSTATUS(status))
            };
            (Some(EditorReport::EditorDone { tree }), end)
        }
    };
    if let (Some(report), Some(channel)) = (report, channel.as_mut()) {
        send_report(channel, report, deadline, seams);
    }
    seams.trace(&format!("exit {end:?}"));
    end
}

/// SIGINT and SIGQUIT ignored in the helper while its editor runs (core
/// §5.3): the terminal sends them to the whole foreground group.
fn ignore_terminal_signals() {
    // SAFETY: dispositions of this process; the editor gets its own back.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
}

/// `word` as `execvp` would find it on `path_var` (`/bin:/usr/bin` when
/// unset): itself when it holds a `/`, else the first executable regular
/// file of that name. An errno when there is none.
fn resolve(word: &OsStr, path_var: Option<&OsStr>) -> Result<CString, i32> {
    let executable = |path: &Path| {
        CString::new(path.as_os_str().as_bytes()).ok().filter(|c| {
            std::fs::metadata(path).is_ok_and(|m| m.is_file())
                    // SAFETY: a NUL-terminated path.
                    && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
        })
    };
    if word.is_empty() {
        return Err(libc::ENOENT);
    }
    if word.as_bytes().contains(&b'/') {
        return CString::new(word.as_bytes()).map_err(|_| libc::EINVAL);
    }
    let path_var = path_var.unwrap_or(OsStr::new("/bin:/usr/bin"));
    path_var
        .as_bytes()
        .split(|b| *b == b':')
        .map(|dir| {
            if dir.is_empty() {
                Path::new(".").join(word)
            } else {
                Path::new(OsStr::from_bytes(dir)).join(word)
            }
        })
        .find_map(|path| executable(&path))
        .ok_or(libc::ENOENT)
}

/// Start the editor (core §5.2): a child with the inherited mask and
/// dispositions, SIGPIPE at its default, the given environment and no
/// descriptor of the helper's (each is close-on-exec). An errno when it
/// couldn't be started, the child having been reaped.
fn spawn(
    argv: &[OsString],
    env: &[(OsString, OsString)],
    path_var: Option<&OsStr>,
    inherited: &Inherited,
    seams: &seam::Seams,
) -> Result<libc::pid_t, i32> {
    if let Some(errno) = seams.spawn_errno() {
        return Err(errno);
    }
    let program = resolve(&argv[0], path_var)?;
    let cstrings = |items: &mut dyn Iterator<Item = Vec<u8>>| -> Result<Vec<CString>, i32> {
        items
            .map(|bytes| CString::new(bytes).map_err(|_| libc::EINVAL))
            .collect()
    };
    let args = cstrings(&mut argv.iter().map(|a| a.as_bytes().to_vec()))?;
    let envs = cstrings(&mut env.iter().map(|(k, v)| {
        let mut entry = k.as_bytes().to_vec();
        entry.push(b'=');
        entry.extend_from_slice(v.as_bytes());
        entry
    }))?;
    let mut arg_ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    arg_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());
    let mut pipe = [0 as RawFd; 2];
    // SAFETY: room for two descriptors.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    // SAFETY: both ends were just made and are owned here.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
    seams.trace(&format!("spawn tasks {}", seam::tasks()));
    // SAFETY: the helper is single-threaded (it runs before anything starts
    // a thread), so the child may run any code; it runs only
    // async-signal-safe calls on memory prepared above, then `execve` or
    // `_exit`.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    if child == 0 {
        // SAFETY: as above; nothing here allocates or unwinds.
        unsafe {
            for (sig, action) in &inherited.actions {
                libc::sigaction(*sig, action, std::ptr::null_mut());
            }
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            seam::child_defaults();
            libc::sigprocmask(libc::SIG_SETMASK, &inherited.mask, std::ptr::null_mut());
            libc::execve(program.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr());
            let errno = *libc::__errno_location();
            let bytes = errno.to_ne_bytes();
            libc::write(write_end.as_raw_fd(), bytes.as_ptr().cast(), bytes.len());
            libc::_exit(SPAWN_FAILED_STATUS);
        }
    }
    drop(write_end);
    let mut bytes = [0u8; 4];
    let mut got = 0;
    while got < bytes.len() {
        // SAFETY: the pipe's read end and the unfilled rest of `bytes`.
        let n = unsafe {
            libc::read(
                read_end.as_raw_fd(),
                bytes[got..].as_mut_ptr().cast(),
                bytes.len() - got,
            )
        };
        if n > 0 {
            got += n as usize;
        } else if n == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    if got == bytes.len() {
        let mut status = 0;
        // SAFETY: reaping the child that failed to exec.
        while unsafe { libc::waitpid(child, &mut status, 0) } < 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        return Err(i32::from_ne_bytes(bytes));
    }
    Ok(child)
}

/// Reap the editor (core §5.3): `waitpid` retried on `EINTR`; any other
/// failure is `WaitUnknown`, its errno returned.
fn wait_editor(child: libc::pid_t, seams: &seam::Seams) -> Result<libc::c_int, i32> {
    if let Some(errno) = seams.wait_errno() {
        seams.trace(&format!("wait-unknown {errno}"));
        return Err(errno);
    }
    loop {
        let mut status = 0;
        // SAFETY: waiting for this process's own child.
        let rc = unsafe { libc::waitpid(child, &mut status, 0) };
        if rc == child {
            return Ok(status);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            errno => {
                let errno = errno.unwrap_or(libc::EIO);
                seams.trace(&format!("wait-unknown {errno}"));
                return Err(errno);
            }
        }
    }
}

/// Account for the editor's descendants (amendment 3 §3.2 step 3,
/// amendment 4 §7.5): reap what has exited until `ECHILD` (clean) or a live
/// child (survivors), within [`REAP_BUDGET`] of `D`'s start and
/// [`REAP_MAX`] reaps, anything else being unknown.
fn reap_tree(deadline: Deadline) -> EditorTree {
    let started = Instant::now();
    for _ in 0..=REAP_MAX {
        if started.elapsed() >= REAP_BUDGET || deadline.remaining().is_none() {
            return EditorTree::Unknown;
        }
        let mut status = 0;
        // SAFETY: a nonblocking wait for any child of this process.
        let rc = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
        match rc {
            0 => return EditorTree::Survivors,
            n if n > 0 => {}
            _ => match io::Error::last_os_error().raw_os_error() {
                Some(libc::ECHILD) => return EditorTree::Clean,
                Some(libc::EINTR) => {}
                _ => return EditorTree::Unknown,
            },
        }
    }
    EditorTree::Unknown
}

/// Send the report by `D`, then, for a clean tree on a daemon that takes a
/// frontier, wait for its acknowledgement by `D` (amendment 4 §7.5). A step
/// with no time left is skipped; a frame not out by `D` counts as no report.
fn send_report(
    channel: &mut ReportChannel,
    report: EditorReport,
    deadline: Deadline,
    seams: &seam::Seams,
) {
    if deadline.remaining().is_none() {
        seams.trace("skip report");
        return;
    }
    seams.sndbuf(channel.conn.stream.as_raw_fd());
    let Ok(mut line) = serde_json::to_vec(&report) else {
        return;
    };
    line.resize(line.len() + seams.pad(), b' ');
    if channel.conn.send_line(&line, deadline, seams).is_err() {
        seams.trace("report unsent");
        return;
    }
    seams.trace("report sent");
    if channel.frontier
        && report
            == (EditorReport::EditorDone {
                tree: EditorTree::Clean,
            })
    {
        let acked = channel.conn.read_line(deadline, seams).is_ok();
        seams.trace(&format!("ack {acked}"));
    }
}

/// The e2e build's seams (plan Q35-12, amendment 6 §1.5 and §2.4), read from
/// the environment at the helper's start. A build without `e2e` has the
/// no-op twin below, which reads nothing.
#[cfg(feature = "e2e")]
pub mod seam {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::RawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use super::Deadline;

    /// Append one line per event to this file: `<CLOCK_MONOTONIC ns> <event>`.
    pub const TRACE: &str = "DOT_AGENT_DECK_E2E_HELPER_TRACE";
    /// A FIFO the helper blocks on, at the points [`PAUSE_AT`] names, until a
    /// byte is written to it.
    pub const PAUSE: &str = "DOT_AGENT_DECK_E2E_HELPER_PAUSE";
    /// Comma-separated points: `start`, `post-editor`, `mutant`.
    pub const PAUSE_AT: &str = "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT";
    /// Starting the editor fails with this errno, and nothing is started.
    pub const SPAWN_ERRNO: &str = "DOT_AGENT_DECK_E2E_HELPER_SPAWN_ERRNO";
    /// The editor's wait fails with this errno after a successful start.
    pub const WAIT_ERRNO: &str = "DOT_AGENT_DECK_E2E_HELPER_WAIT_ERRNO";
    /// The report socket's `SO_SNDBUF`, set before the report.
    pub const SNDBUF: &str = "DOT_AGENT_DECK_E2E_HELPER_SNDBUF";
    /// Blanks added to the report frame before its newline.
    pub const PAD: &str = "DOT_AGENT_DECK_E2E_HELPER_PAD";
    /// `1`: SIGUSR1 gets a no-op handler without `SA_RESTART`, so a test can
    /// interrupt the helper's calls.
    pub const EINTR: &str = "DOT_AGENT_DECK_E2E_HELPER_EINTR";
    /// `a5-alarm`: after a failed start, run amendment 5 §3.4's diagnostic
    /// write, the history amendment 6 removed: a no-op SIGALRM handler, the
    /// timer, then one write(2) to fd 2. A RED control, never the design.
    pub const MUTANT: &str = "DOT_AGENT_DECK_E2E_HELPER_MUTANT";

    /// The mutant's diagnostic line.
    pub const MUTANT_LINE: &[u8] = b"dot-agent-deck: could not start the editor\n";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Point {
        Start,
        PostEditor,
        Mutant,
    }

    impl Point {
        fn word(self) -> &'static str {
            match self {
                Point::Start => "start",
                Point::PostEditor => "post-editor",
                Point::Mutant => "mutant",
            }
        }
    }

    pub struct Seams {
        trace: Option<std::fs::File>,
        pause: Option<CString>,
        pause_at: Vec<String>,
        spawn_errno: Option<i32>,
        wait_errno: Option<i32>,
        sndbuf: Option<i32>,
        pad: usize,
        mutant: bool,
    }

    static ALARMED: AtomicBool = AtomicBool::new(false);

    extern "C" fn noop(_: libc::c_int) {}

    extern "C" fn alarmed(_: libc::c_int) {
        ALARMED.store(true, Ordering::SeqCst);
    }

    /// A handler without `SA_RESTART` for `sig`.
    fn interrupting(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
        // SAFETY: a zeroed sigaction with a valid handler and an empty mask.
        unsafe {
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = handler as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }

    fn number<T: std::str::FromStr>(name: &str) -> Option<T> {
        std::env::var(name).ok()?.parse().ok()
    }

    impl Seams {
        pub fn read() -> Seams {
            if std::env::var(EINTR).as_deref() == Ok("1") {
                interrupting(libc::SIGUSR1, noop);
            }
            Seams {
                trace: std::env::var_os(TRACE).and_then(|path| {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .ok()
                }),
                pause: std::env::var(PAUSE).ok().and_then(|p| CString::new(p).ok()),
                pause_at: std::env::var(PAUSE_AT)
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_string)
                    .collect(),
                spawn_errno: number(SPAWN_ERRNO),
                wait_errno: number(WAIT_ERRNO),
                sndbuf: number(SNDBUF),
                pad: number(PAD).unwrap_or(0),
                mutant: std::env::var(MUTANT).as_deref() == Ok("a5-alarm"),
            }
        }

        pub fn trace(&self, event: &str) {
            if let Some(mut file) = self.trace.as_ref() {
                let _ = writeln!(file, "{} {event}", mono_ns());
            }
        }

        /// Block until the test writes a byte to the pause FIFO, when `at`
        /// is one of the configured points. An interrupted open or read is
        /// retried, and a SIGALRM handled on the way is traced once.
        pub fn pause(&self, at: Point) {
            let (Some(fifo), true) = (&self.pause, self.pause_at.iter().any(|p| p == at.word()))
            else {
                return;
            };
            self.trace(&format!("pause {}", at.word()));
            let mut traced = false;
            let mut note = |seams: &Seams| {
                if !traced && ALARMED.load(Ordering::SeqCst) {
                    traced = true;
                    seams.trace("alarm-handler-ran");
                }
            };
            let fd: RawFd = loop {
                // SAFETY: a NUL-terminated path.
                let fd = unsafe { libc::open(fifo.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
                if fd >= 0 {
                    break fd;
                }
                note(self);
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    return;
                }
            };
            let mut byte = 0u8;
            loop {
                // SAFETY: the FIFO just opened and one byte of room.
                let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
                note(self);
                if n >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
            // SAFETY: the descriptor opened above, closed once.
            unsafe { libc::close(fd) };
            self.trace(&format!("resume {}", at.word()));
        }

        pub fn spawn_errno(&self) -> Option<i32> {
            self.spawn_errno
        }

        pub fn wait_errno(&self) -> Option<i32> {
            self.wait_errno
        }

        pub fn sndbuf(&self, fd: RawFd) {
            if let Some(size) = self.sndbuf {
                // SAFETY: a valid socket and an int option value.
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_SNDBUF,
                        (&size as *const i32).cast(),
                        size_of::<i32>() as libc::socklen_t,
                    )
                };
            }
        }

        pub fn pad(&self) -> usize {
            self.pad
        }

        /// The A5 mutant, when configured: a no-op SIGALRM handler without
        /// `SA_RESTART`, the timer for what is left to `deadline`, a pause
        /// in which that handler runs, then one write(2) of a line to fd 2,
        /// which nothing interrupts any more.
        pub fn mutant_a5(&self, deadline: Deadline) {
            if !self.mutant {
                return;
            }
            interrupting(libc::SIGALRM, alarmed);
            super::set_alarm(
                deadline
                    .remaining()
                    .unwrap_or_default()
                    .max(std::time::Duration::from_millis(1)),
            );
            self.pause(Point::Mutant);
            // SAFETY: fd 2 and a buffer of the line's length.
            unsafe { libc::write(2, MUTANT_LINE.as_ptr().cast(), MUTANT_LINE.len()) };
            super::set_alarm(std::time::Duration::ZERO);
            // SAFETY: SIGALRM's disposition, back to its default.
            unsafe { libc::signal(libc::SIGALRM, libc::SIG_DFL) };
        }
    }

    /// In the editor child, before `execve`: a seam's handler back to its
    /// default. Async-signal-safe.
    pub fn child_defaults() {
        // SAFETY: a disposition of the child process.
        unsafe { libc::signal(libc::SIGUSR1, libc::SIG_DFL) };
    }

    /// `CLOCK_MONOTONIC` now, in nanoseconds.
    pub fn mono_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid clock and room for the result.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    /// `at` in [`mono_ns`]'s clock. `Instant` is `CLOCK_MONOTONIC` on Linux.
    pub fn mono_ns_at(at: Instant) -> u64 {
        let now = Instant::now();
        let ns = mono_ns();
        ns.saturating_sub(now.saturating_duration_since(at).as_nanos() as u64)
    }

    /// How many threads this process has.
    pub fn tasks() -> usize {
        std::fs::read_dir("/proc/self/task").map_or(0, Iterator::count)
    }
}

/// The production twin of the e2e seams: no environment is read, and every
/// call does nothing.
#[cfg(not(feature = "e2e"))]
mod seam {
    use std::os::fd::RawFd;
    use std::time::Instant;

    use super::Deadline;

    #[derive(Debug, Clone, Copy)]
    pub enum Point {
        Start,
        PostEditor,
    }

    pub struct Seams;

    impl Seams {
        #[inline]
        pub fn read() -> Seams {
            Seams
        }
        #[inline]
        pub fn trace(&self, _: &str) {}
        #[inline]
        pub fn pause(&self, _: Point) {}
        #[inline]
        pub fn spawn_errno(&self) -> Option<i32> {
            None
        }
        #[inline]
        pub fn wait_errno(&self) -> Option<i32> {
            None
        }
        #[inline]
        pub fn sndbuf(&self, _: RawFd) {}
        #[inline]
        pub fn pad(&self) -> usize {
            0
        }
        #[inline]
        pub fn mutant_a5(&self, _: Deadline) {}
    }

    #[inline]
    pub fn child_defaults() {}

    #[inline]
    pub fn mono_ns_at(_: Instant) -> u64 {
        0
    }

    #[inline]
    pub fn tasks() -> usize {
        0
    }
}
