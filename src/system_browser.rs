//! PRD #1401: the TUI's `o` key — open the selected agent's pull request in the
//! system browser.
//!
//! The URL comes off the wire, from a daemon that may be remote and is not
//! trusted, and this is the one place such a URL reaches a process spawn. Two
//! independent defenses keep it from ever being run as code:
//!
//! * [`canonical_pull_request_url`] parses it and accepts only
//!   `https://github.com/<owner>/<repo>/pull/<number>` — the same shape the
//!   desktop's in-app browser opens on — and rebuilds the URL from those four
//!   parts, each restricted to characters GitHub allows in them. No query,
//!   fragment, credentials, port or percent-escape survives into the result.
//! * [`browser_command`] treats `$BROWSER` as a program and its arguments,
//!   never as shell source, and refuses to substitute the URL (`%s`) into a
//!   template that runs a shell or another interpreter, where the substituted
//!   argument would be the interpreter's program text.
//!
//! When no browser can be launched — a failure, or a host with no display to
//! launch one on, which is the usual case for a TUI on a remote host under
//! `dot-agent-deck connect` — the status line shows the URL instead, so the
//! user can copy it ([`open_pull_request`]).

use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// How long a launched browser or opener is polled for a failing exit before
/// the launch is reported ([`watch_launch`]). A polling policy, not a
/// wall-clock bound: the last poll can land up to one poll interval past it,
/// later on a starved machine. A browser that is still running then — one
/// that stays in the foreground — counts as launched, and is handed to the
/// shared reaper ([`reap_in_background`]).
pub const LAUNCH_GRACE: Duration = Duration::from_millis(500);

/// How many launches may be watched at once, across the process. A launch
/// beyond it starts no browser and says so ([`TOO_MANY_LAUNCHES`]); a watch
/// lasts about [`LAUNCH_GRACE`], so the next attempt a moment later succeeds.
pub const MAX_WATCHED_LAUNCHES: usize = 4;

/// Why a launch beyond [`MAX_WATCHED_LAUNCHES`] was refused.
pub const TOO_MANY_LAUNCHES: &str = "earlier launches are still starting; try again in a moment";

/// How often the reaper checks the children it holds for an exit.
const REAP_POLL: Duration = Duration::from_millis(250);

/// A count of launches being watched, capped. [`Self::claim`] answers a slot
/// that is released when it is dropped, or `None` at the cap.
pub struct LaunchSlots {
    in_use: AtomicUsize,
    max: usize,
}

/// One claimed [`LaunchSlots`] slot, released on drop.
pub struct LaunchSlot<'a>(&'a LaunchSlots);

impl LaunchSlots {
    pub const fn new(max: usize) -> Self {
        Self {
            in_use: AtomicUsize::new(0),
            max,
        }
    }

    /// A slot, or `None` when `max` are already claimed.
    pub fn claim(&self) -> Option<LaunchSlot<'_>> {
        self.in_use
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .ok()
            .map(|_| LaunchSlot(self))
    }
}

impl Drop for LaunchSlot<'_> {
    fn drop(&mut self) {
        self.0.in_use.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The process's launch watches: the TUI's `o` and the desktop's hand-offs
/// both claim from it.
static LAUNCH_SLOTS: LaunchSlots = LaunchSlots::new(MAX_WATCHED_LAUNCHES);

/// The one reaper thread's queue, started on first use. `None` when the
/// thread could not be started.
static REAPER: OnceLock<Option<Sender<Child>>> = OnceLock::new();

/// Hand `child`, still running, to the process's one reaper thread, which
/// waits for every child it is given — however many — so none lingers as a
/// zombie and none costs a thread of its own. If that thread could not be
/// started, the child is dropped unreaped: it keeps running, and on Unix
/// stays a zombie after it exits, until this process does.
pub fn reap_in_background(child: Child) {
    let reaper = REAPER.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("browser-reaper".into())
            .spawn(move || reap(rx))
        {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!(error = %e, "could not start the browser reaper thread");
                None
            }
        }
    });
    if let Some(tx) = reaper {
        let _ = tx.send(child);
    }
}

/// The reaper's loop: hold every child it is sent and poll each for an exit
/// every [`REAP_POLL`], dropping it once it has one. Blocks without polling
/// while it holds none.
fn reap(rx: Receiver<Child>) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        let next = if children.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(REAP_POLL)
        };
        match next {
            Ok(child) => children.push(child),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) if children.is_empty() => return,
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(REAP_POLL),
        }
        children.extend(rx.try_iter());
        // An unreadable status will not become readable: stop holding it.
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

/// The `https://github.com/<owner>/<repo>/pull/<number>` form of `raw`, rebuilt
/// from its validated parts, or `None` when `raw` is anything else.
pub fn canonical_pull_request_url(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw).ok()?;
    if url.scheme() != "https"
        || !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    let [owner, repo, "pull", number] = segments.as_slice() else {
        return None;
    };
    // GitHub owners are letters, digits and `-`; repositories add `_` and
    // `.`. Anything else — including a percent-escape — is not a GitHub
    // repository, so it is refused rather than re-encoded.
    let owner_ok = !owner.is_empty()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = !repo.is_empty()
        && !matches!(*repo, "." | "..")
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !owner_ok || !repo_ok || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u64 = number.parse().ok()?;
    Some(format!("https://github.com/{owner}/{repo}/pull/{number}"))
}

/// Program names whose arguments can be program text (`sh -c <source>`,
/// `python -c`, `node -e`, `cmd /c`, …). A `%s` in a template naming one of
/// these anywhere — directly or behind a wrapper such as `env` — is refused.
fn is_interpreter(word: &str) -> bool {
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let base = base.to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    const NAMES: &[&str] = &[
        "sh",
        "ash",
        "bash",
        "dash",
        "zsh",
        "ksh",
        "mksh",
        "yash",
        "fish",
        "csh",
        "tcsh",
        "busybox",
        "env",
        "nohup",
        "setsid",
        "exec",
        "eval",
        "xargs",
        "perl",
        "ruby",
        "node",
        "nodejs",
        "deno",
        "bun",
        "php",
        "lua",
        "tclsh",
        "wish",
        "awk",
        "gawk",
        "mawk",
        "osascript",
        "cmd",
        "powershell",
        "pwsh",
        "wscript",
        "cscript",
    ];
    NAMES.contains(&base) || base.starts_with("python") || base.starts_with("perl")
}

/// The program and arguments that open `url` per `browser`, a `$BROWSER`
/// value: its first `:`-separated entry, split on whitespace into a program
/// and arguments, with `%s` replaced by the URL or, without one, the URL
/// appended as the last argument. `Ok(None)` means `browser` names no program,
/// so the platform opener applies.
///
/// Refused: a `%s` in a template that runs an interpreter (see
/// [`is_interpreter`]), because there the substituted argument is program
/// text. Such a template without `%s` gets the URL as a trailing positional
/// argument. With the template's own source before it (`sh -c open-it`) an
/// interpreter reads that argument as data (`$0`/`$1`, `sys.argv`); without
/// (`sh -c`) the URL itself becomes the source. Either way it is the URL
/// [`canonical_pull_request_url`] rebuilt, which carries no character a shell
/// treats specially.
pub fn browser_command(browser: &str, url: &str) -> Result<Option<(String, Vec<String>)>, String> {
    let command = browser.split(':').next().unwrap_or("").trim();
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return Ok(None);
    };
    let mut args: Vec<String> = words.map(str::to_string).collect();
    if args.iter().any(|arg| arg.contains("%s")) {
        if is_interpreter(program) || args.iter().any(|arg| is_interpreter(arg)) {
            return Err(format!(
                "BROWSER ({program}) would run the URL as a script; set it to a browser program"
            ));
        }
        for arg in &mut args {
            *arg = arg.replace("%s", url);
        }
    } else {
        args.push(url.to_string());
    }
    Ok(Some((program.to_string(), args)))
}

/// Spawn `program` with `args`, detached from the terminal's stdio.
pub fn spawn_browser(program: &str, args: &[String]) -> Result<Child, String> {
    spawn_detached(Command::new(program).args(args)).map_err(|e| format!("{program}: {e}"))
}

/// Spawn `command` with no stdio and, on Unix, in a process group of its own,
/// so a signal the terminal sends the deck's foreground group does not reach
/// the browser. Still this process's child, so [`watch_launch`] can read its
/// exit status.
fn spawn_detached(command: &mut Command) -> std::io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    command.spawn()
}

/// How a launch that did not fail went: the program exited successfully, or it
/// was still running when the watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launched {
    /// Exited with status 0 within the grace period.
    Exited,
    /// Still running when the grace period ended.
    Running,
}

/// Watch `child`, launched as `program`, polling it for `grace` (a polling
/// policy, see [`LAUNCH_GRACE`]): a non-zero exit in that time is a failure,
/// an exit of 0 or a child still running is a launch. A child still running
/// is handed to the shared reaper ([`reap_in_background`]), so a browser that
/// stays in the foreground neither holds up the caller much past `grace` nor
/// lingers as a zombie.
pub fn watch_launch(mut child: Child, program: &str, grace: Duration) -> Result<Launched, String> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(Launched::Exited),
            Ok(Some(status)) => return Err(format!("{program} failed ({status})")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Still running, or its status cannot be read: nothing says it
            // failed, so it counts as launched.
            Ok(None) | Err(_) => {
                reap_in_background(child);
                return Ok(Launched::Running);
            }
        }
    }
}

/// Open `url` with the platform's opener (`xdg-open` and its fallbacks,
/// `open`, `start`), the first one that can be started, watched like a
/// `BROWSER` program ([`watch_launch`]) on the caller's thread. Counted
/// against [`MAX_WATCHED_LAUNCHES`]: beyond it, nothing is started and the
/// answer is [`TOO_MANY_LAUNCHES`].
pub fn open_with_platform_opener(url: &str, grace: Duration) -> Result<Launched, String> {
    let _slot = LAUNCH_SLOTS
        .claim()
        .ok_or_else(|| TOO_MANY_LAUNCHES.to_string())?;
    let (child, program) = spawn_platform_opener(url)?;
    watch_launch(child, &program, grace)
}

/// Start the first of the platform's openers for `url` that can be started,
/// and name it.
fn spawn_platform_opener(url: &str) -> Result<(Child, String), String> {
    let mut last_error = None;
    for mut command in open::commands(url) {
        let program = command.get_program().to_string_lossy().into_owned();
        match spawn_detached(&mut command) {
            Ok(child) => return Ok((child, program)),
            Err(e) => last_error = Some(format!("{program}: {e}")),
        }
    }
    Err(last_error.unwrap_or_else(|| "no opener for this platform".to_string()))
}

/// What [`open_pull_request`] needs to know about the host.
#[derive(Debug, Clone, Default)]
pub struct BrowserEnv {
    /// `$BROWSER`, when set.
    pub browser: Option<String>,
    /// Whether the platform opener has a display to open a browser on: always
    /// on macOS and Windows, and elsewhere only with `DISPLAY` or
    /// `WAYLAND_DISPLAY` set.
    pub display: bool,
}

impl BrowserEnv {
    /// This process's environment.
    pub fn from_process() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Self {
            browser: std::env::var("BROWSER")
                .ok()
                .filter(|b| !b.trim().is_empty()),
            display: cfg!(any(target_os = "macos", windows))
                || set("DISPLAY")
                || set("WAYLAND_DISPLAY"),
        }
    }
}

/// Start opening the pull request at `raw_url` and return the status-line
/// message to show now. The browser is spawned on the caller's thread, and
/// waiting for its exit happens off it.
///
/// When a program was launched, that message is `Opening <url> in the
/// browser`, and `on_settled` is called once, from another thread, with the
/// message for how the launch went: watched for about [`LAUNCH_GRACE`], a
/// program that exited successfully reports "Opened", one that exited
/// non-zero reports the could-not-open message with the URL, and one still
/// running keeps "Opening … in the browser", since a browser that stays in
/// the foreground never says whether the page loaded. When nothing was
/// launched — the URL was refused, there is no display, or no program could
/// be started, [`MAX_WATCHED_LAUNCHES`] launches are already being watched,
/// or no thread could be started to watch it — the returned message is
/// already the outcome, and `on_settled` is dropped uncalled.
///
/// Whenever no browser opens, the message leads with the URL, so a user on a
/// host with no browser can copy it.
pub fn open_pull_request(
    raw_url: &str,
    env: &BrowserEnv,
    on_settled: impl FnOnce(String) + Send + 'static,
) -> String {
    open_pull_request_in(&LAUNCH_SLOTS, raw_url, env, on_settled)
}

/// [`open_pull_request`], counting its watch against `slots`.
fn open_pull_request_in(
    slots: &'static LaunchSlots,
    raw_url: &str,
    env: &BrowserEnv,
    on_settled: impl FnOnce(String) + Send + 'static,
) -> String {
    let Some(url) = canonical_pull_request_url(raw_url) else {
        return "Could not open the pull request: not a GitHub pull request URL".to_string();
    };
    let browser = match env.browser.as_deref().map(|b| browser_command(b, &url)) {
        Some(Ok(Some(command))) => Some(command),
        Some(Err(e)) => return launch_message(&url, Err(e)),
        None | Some(Ok(None)) if !env.display => {
            return format!("Pull request: {url} (no display here to open a browser on)");
        }
        None | Some(Ok(None)) => None,
    };
    let Some(slot) = slots.claim() else {
        return launch_message(&url, Err(TOO_MANY_LAUNCHES.to_string()));
    };
    // The watcher starts before the browser does, so a thread that cannot be
    // started leaves nothing launched and unwatched. It holds the slot until
    // its watch ends, and exits uncalled if no child ever reaches it.
    let (tx, rx) = mpsc::channel::<(Child, String)>();
    let watched_url = url.clone();
    let watcher = std::thread::Builder::new()
        .name("browser-watch".into())
        .spawn(move || {
            let Ok((child, program)) = rx.recv() else {
                return;
            };
            let launched = watch_launch(child, &program, LAUNCH_GRACE);
            // The watch is over: free the slot before reporting it.
            drop(slot);
            on_settled(launch_message(&watched_url, launched));
        });
    if let Err(e) = watcher {
        return launch_message(&url, Err(format!("could not watch the launch: {e}")));
    }
    let spawned = match browser {
        Some((program, args)) => spawn_browser(&program, &args).map(|child| (child, program)),
        None => spawn_platform_opener(&url),
    };
    match spawned {
        Ok(spawned) => {
            // The watcher waits on `rx` until this send or the drop of `tx`,
            // so the send fails only if that thread died first; the child
            // then comes back in the error and is reaped here.
            if let Err(mpsc::SendError((child, _))) = tx.send(spawned) {
                reap_in_background(child);
            }
            launch_message(&url, Ok(Launched::Running))
        }
        Err(e) => launch_message(&url, Err(e)),
    }
}

/// The status-line message for how opening `url` went.
fn launch_message(url: &str, launched: Result<Launched, String>) -> String {
    match launched {
        Ok(Launched::Exited) => format!("Opened {url}"),
        Ok(Launched::Running) => format!("Opening {url} in the browser"),
        Err(e) => format!("Pull request: {url} (could not open a browser: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_github_pull_request_url_is_accepted_and_it_is_rebuilt() {
        for (raw, want) in [
            (
                "https://github.com/vfarcic/dot-agent-deck/pull/1401",
                "https://github.com/vfarcic/dot-agent-deck/pull/1401",
            ),
            (
                "https://GitHub.com/o/r.js/pull/007",
                "https://github.com/o/r.js/pull/7",
            ),
            (
                "https://github.com/o/my_repo/pull/1?x=1#y",
                "https://github.com/o/my_repo/pull/1",
            ),
        ] {
            assert_eq!(
                canonical_pull_request_url(raw).as_deref(),
                Some(want),
                "{raw}"
            );
        }
        for bad in [
            "https://github.com/o/r/pull/1;printf${IFS}X",
            "https://github.com/o/r/pull/1$(touch x)",
            "https://github.com/o;id/r/pull/1",
            "https://github.com/o/r%60id%60/pull/1",
            "https://github.com/o/r/pull/1/files",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/abc",
            "https://github.com/o/r/pull/",
            "https://github.com/o/../pull/1",
            "https://github.com.evil.example/o/r/pull/1",
            "https://gist.github.com/o/r/pull/1",
            "http://github.com/o/r/pull/1",
            "https://user:pass@github.com/o/r/pull/1",
            "https://github.com:8443/o/r/pull/1",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert_eq!(canonical_pull_request_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_plain_browser_gets_the_url_as_one_argument() {
        let url = "https://github.com/o/r/pull/1";
        assert_eq!(
            browser_command("firefox", url).unwrap(),
            Some(("firefox".into(), vec![url.into()]))
        );
        assert_eq!(
            browser_command("chromium --new-window %s:firefox", url).unwrap(),
            Some(("chromium".into(), vec!["--new-window".into(), url.into()]))
        );
        assert_eq!(browser_command("  ", url).unwrap(), None);
    }

    #[test]
    fn a_url_is_never_substituted_into_an_interpreters_program_text() {
        let url = "https://github.com/o/r/pull/1";
        for template in [
            "sh -c %s",
            "/bin/bash -c %s",
            "env sh -c %s",
            "/usr/bin/env bash -c %s",
            "python3 -c %s",
            "node -e %s",
            "cmd.exe /c start %s",
            "powershell -Command %s",
        ] {
            assert!(browser_command(template, url).is_err(), "{template}");
        }
        // Without `%s`, the URL is a trailing positional argument — data to the
        // interpreter, not source.
        assert_eq!(
            browser_command("sh -c open-it", url).unwrap(),
            Some(("sh".into(), vec!["-c".into(), "open-it".into(), url.into()]))
        );
    }

    /// What [`open_pull_request`] shows at once for `raw`, and what the status
    /// line ends up showing: the message it delivers when a launch settles,
    /// or, when it launched nothing and so delivers nothing, the first one.
    fn open_and_settle(raw: &str, env: &BrowserEnv) -> (String, String) {
        let (tx, rx) = std::sync::mpsc::channel();
        let shown = open_pull_request(raw, env, move |message| {
            let _ = tx.send(message);
        });
        let settled = match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(message) => message,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => shown.clone(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("a launch of {raw} never settled")
            }
        };
        (shown, settled)
    }

    #[cfg(unix)]
    fn wait_for_file(path: &std::path::Path) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} never written",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A shell-valued `BROWSER` and a hostile URL execute nothing: the payload
    /// would create `pwned`, and it never appears — neither for the hostile URL
    /// nor for a valid one substituted into the shell's `-c` source.
    #[cfg(unix)]
    #[test]
    fn a_shell_valued_browser_with_a_hostile_url_executes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let pwned = dir.path().join("pwned");
        let env = BrowserEnv {
            browser: Some("sh -c %s".into()),
            display: true,
        };
        let hostile = format!(
            "https://github.com/o/r/pull/1;touch${{IFS}}{}",
            pwned.display()
        );
        let (_, msg) = open_and_settle(&hostile, &env);
        assert!(msg.starts_with("Could not open"), "{msg}");
        let (_, msg) = open_and_settle("https://github.com/o/r/pull/1", &env);
        assert!(
            msg.starts_with("Pull request: https://github.com/o/r/pull/1 (could not open"),
            "{msg}"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!pwned.exists(), "the payload ran");
    }

    /// A plain executable receives exactly one argument: the rebuilt URL.
    #[cfg(unix)]
    #[test]
    fn a_plain_executable_browser_receives_exactly_one_url_argument() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv");
        let script = dir.path().join("browser");
        crate::test_isolation::write_script(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|' \"$#\" \"$@\" > '{0}.tmp'\necho >> '{0}.tmp'\nmv '{0}.tmp' '{0}'\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BrowserEnv {
            browser: Some(script.display().to_string()),
            display: false,
        };
        let (shown, msg) = open_and_settle("https://github.com/o/r/pull/12?tab=files", &env);
        assert_eq!(
            shown,
            "Opening https://github.com/o/r/pull/12 in the browser"
        );
        assert_eq!(msg, "Opened https://github.com/o/r/pull/12");
        assert_eq!(
            wait_for_file(&record),
            "1|https://github.com/o/r/pull/12|\n"
        );
    }

    /// Scenario: Configure BROWSER as a direct executable that exits with
    /// status 1. The status line must report failure and retain the PR URL.
    #[cfg(unix)]
    #[test]
    fn a_browser_exiting_nonzero_shows_failure_and_the_url() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("browser");
        crate::test_isolation::write_script(&script, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BrowserEnv {
            browser: Some(script.display().to_string()),
            display: false,
        };
        let (shown, msg) = open_and_settle("https://github.com/o/r/pull/3", &env);
        assert!(
            !shown.starts_with("Opened"),
            "nothing may claim Opened before the browser's exit is known; got {shown}"
        );
        assert!(
            msg.starts_with(
                "Pull request: https://github.com/o/r/pull/3 (could not open a browser:"
            ),
            "a browser that exits 1 must report failure, not Opened; got {msg}"
        );
    }

    /// When the browser cannot be launched, the status line carries the URL so
    /// the user can copy it.
    #[test]
    fn a_failed_launch_shows_the_url() {
        let env = BrowserEnv {
            browser: Some("/nonexistent/dot-agent-deck-test-browser".into()),
            display: true,
        };
        let (shown, msg) = open_and_settle("https://github.com/o/r/pull/3", &env);
        assert_eq!(
            shown, msg,
            "a program that never started has nothing to watch"
        );
        assert!(
            msg.starts_with(
                "Pull request: https://github.com/o/r/pull/3 (could not open a browser:"
            ),
            "{msg}"
        );
    }

    /// The threads this process has right now.
    #[cfg(target_os = "linux")]
    fn thread_count() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    /// Scenario: Launch many browsers that all outlive the grace period. They
    /// are all reaped once they exit, by one shared reaper rather than one
    /// waiting thread each, so the deck's thread count stays bounded.
    #[cfg(target_os = "linux")]
    #[test]
    fn browsers_that_outlive_the_grace_share_one_reaper() {
        const CHILDREN: usize = 64;
        let before = thread_count();
        let mut pids = Vec::new();
        for _ in 0..CHILDREN {
            let child = spawn_browser("sleep", &["1".to_string()]).unwrap();
            pids.push(child.id());
            assert_eq!(
                watch_launch(child, "sleep", Duration::ZERO),
                Ok(Launched::Running)
            );
        }
        let grown = thread_count().saturating_sub(before);
        assert!(
            grown < CHILDREN / 2,
            "{grown} threads were started to wait for {CHILDREN} browsers"
        );
        // Every one is reaped once it exits: no zombie is left behind.
        let deadline = Instant::now() + Duration::from_secs(20);
        for pid in pids {
            let stat = format!("/proc/{pid}/stat");
            loop {
                let zombie_or_running = std::fs::read_to_string(&stat).is_ok();
                if !zombie_or_running {
                    break;
                }
                assert!(Instant::now() < deadline, "browser {pid} was never reaped");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// Scenario: Press `o` while as many launches as the cap allows are still
    /// being watched. The new launch starts no browser and says why, with the
    /// URL; once a watch ends, the next launch opens normally.
    #[cfg(unix)]
    #[test]
    fn a_launch_beyond_the_watch_cap_is_refused_with_the_url() {
        use std::os::unix::fs::PermissionsExt;
        static SLOTS: LaunchSlots = LaunchSlots::new(1);
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv");
        let script = dir.path().join("browser");
        crate::test_isolation::write_script(
            &script,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n", record.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = BrowserEnv {
            browser: Some(script.display().to_string()),
            display: false,
        };

        let held = SLOTS.claim().expect("a free slot");
        let shown = open_pull_request_in(&SLOTS, "https://github.com/o/r/pull/5", &env, |_| {});
        assert_eq!(
            shown,
            format!(
                "Pull request: https://github.com/o/r/pull/5 (could not open a browser: {TOO_MANY_LAUNCHES})"
            )
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(!record.exists(), "a refused launch started a browser");

        drop(held);
        let (tx, rx) = std::sync::mpsc::channel();
        let shown = open_pull_request_in(&SLOTS, "https://github.com/o/r/pull/5", &env, move |m| {
            let _ = tx.send(m);
        });
        assert_eq!(
            shown,
            "Opening https://github.com/o/r/pull/5 in the browser"
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(30)).unwrap(),
            "Opened https://github.com/o/r/pull/5"
        );
        assert!(
            SLOTS.claim().is_some(),
            "the settled watch released its slot"
        );
    }

    /// A host with no display and no `BROWSER` — a TUI on a remote host — opens
    /// nothing and shows the URL instead.
    #[test]
    fn no_display_and_no_browser_shows_the_url() {
        let env = BrowserEnv {
            browser: None,
            display: false,
        };
        assert_eq!(
            open_and_settle("https://github.com/o/r/pull/4", &env).1,
            "Pull request: https://github.com/o/r/pull/4 (no display here to open a browser on)"
        );
    }
}
