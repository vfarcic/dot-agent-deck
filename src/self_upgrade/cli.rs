//! `dot-agent-deck upgrade`: check for a newer release, print the plan for
//! this copy and for the other copy on the machine, and carry out each one the
//! user confirms. It is also the command the clients show where they cannot
//! act themselves ([`super::UPGRADE_COMMAND`]).

use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::detect::{self, CopyKind};
use super::discover::{self, OtherCopy};
use super::execute::{self, ReleaseSource};
use super::plan::{self, PlanOptions, UpgradePlan};
use super::{Host, UpgradeError};

/// What the user asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Args {
    /// Only report; change nothing.
    pub check: bool,
    /// Confirm every upgrade without asking.
    pub yes: bool,
}

/// Where the answers to the confirmation questions come from, when anywhere.
pub enum Answers<'a> {
    /// A terminal: ask, and read the answer.
    Terminal(&'a mut dyn BufRead),
    /// Not a terminal: nothing can be asked.
    None,
}

/// Write `text` as one line, filtered the way the TUI and the desktop app
/// filter what they show: control and bidi formatting characters are dropped,
/// so a path or a subprocess's message cannot move the cursor, clear the
/// screen or reorder the line. Commands go through [`say_items`], which never
/// rewrites one.
fn say(out: &mut dyn Write, text: &str) {
    let _ = writeln!(
        out,
        "{}",
        crate::untrusted_text::strip_control_and_bidi(text, false)
    );
}

/// What the CLI prints in place of a command its output filter would change.
pub const COMMAND_NOT_SHOWN: &str =
    "(A command is left out here, because it contains characters that cannot be shown safely.)";

/// Write `items` one line each: prose as [`say`] writes it, and a command
/// indented and exactly as the core built it. The core builds no command that
/// [`say`]'s filter would change, so one that it would is not printed in any
/// form, rewritten or not: [`COMMAND_NOT_SHOWN`] says it was left out.
fn say_items(out: &mut dyn Write, items: &[plan::PlanLine]) {
    for item in items {
        match item {
            plan::PlanLine::Text(text) => say(out, text),
            plan::PlanLine::Command(command)
                if crate::untrusted_text::strip_control_and_bidi(command, false) == *command =>
            {
                let _ = writeln!(out, "{}", item.render());
            }
            plan::PlanLine::Command(_) => say(out, COMMAND_NOT_SHOWN),
        }
    }
}

/// What the CLI prints when the user interrupted an upgrade.
pub const CANCELLED: &str = "Cancelled.";

/// The user's interruption of [`run`]: a Ctrl+C (`SIGINT`) or `SIGTERM` the
/// CLI receives while it upgrades a copy.
///
/// Sticky: once set it stays set for the rest of the run, so no command is
/// started after it ([`CliHost`]) and no further copy is attempted ([`run`]).
/// Clones share the flag.
#[derive(Clone, Default)]
pub struct Interrupt {
    set: Arc<AtomicBool>,
    /// Whether [`Self::listen`] catches the two signals: the CLI's does; one
    /// a test makes with `default()` is set only by the test.
    signals: bool,
}

impl Interrupt {
    /// The CLI's: set by `SIGINT` or `SIGTERM` while a copy is upgraded.
    pub fn from_signals() -> Self {
        Self {
            set: Arc::default(),
            signals: true,
        }
    }

    /// Whether the user interrupted the run, now or earlier.
    pub fn is_set(&self) -> bool {
        #[cfg(unix)]
        if self.signals && forward::received() {
            self.set.store(true, Ordering::SeqCst);
        }
        self.set.load(Ordering::SeqCst)
    }

    // Used only by Unix-gated tests: native Windows is unsupported (#164).
    #[cfg(all(test, unix))]
    fn set(&self) {
        self.set.store(true, Ordering::SeqCst);
    }

    /// Catch `SIGINT` and `SIGTERM` (when this catches signals) until the
    /// returned guard is dropped, which puts the previous dispositions back.
    /// Read [`Self::is_set`] after dropping it: a signal that arrives while
    /// the dispositions are put back is caught and only seen then.
    fn listen(&self) -> Listening {
        #[cfg(unix)]
        {
            Listening {
                _forwarding: self.signals.then(forward::Forwarding::install),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.signals;
            Listening {}
        }
    }

    /// Resolves once [`Self::is_set`]; asked every 50 ms.
    async fn until_set(&self) {
        while !self.is_set() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// [`Interrupt::listen`]'s guard.
struct Listening {
    #[cfg(unix)]
    _forwarding: Option<forward::Forwarding>,
}

/// The CLI's [`Host`]: [`super::SystemHost`], with the user's interruption
/// ([`Interrupt`]) forwarded to the command it runs as a cancellation
/// ([`super::SystemHost::run_within_cancellable`]), and refused afterwards.
///
/// The command runs in a session of its own, so the terminal's `^C` reaches
/// only the CLI, which [`run`] catches while it upgrades a copy; without
/// that, the CLI would die and leave the command running with nothing left to
/// bound it. While a command runs, an interruption stops the command's group,
/// waits up to its grace for it to exit, and says it was cancelled. Once the
/// run is interrupted:
///
/// - a command is not started at all ([`super::cancelled_before_start`]),
///   except a cleanup command ([`Host::run_cleanup`]: the disk-image detach,
///   and the `dpkg-deb`/`dpkg-query` reads of a stopped `.deb` install), which
///   still runs, bounded but not cancellable;
/// - a command that exited successfully is reported as cancelled
///   ([`super::cancelled_after_exit`]) rather than as its success, so the
///   upgrade never acts on a check whose run the user cancelled, even one
///   cancelled after the command exited, while its output was still read.
///
/// Nothing here signals a command: a cancellation reaches one only through
/// the runner, before the runner has reaped it.
pub struct CliHost {
    pub inner: super::SystemHost,
    pub interrupt: Interrupt,
}

impl Host for CliHost {
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<super::CommandOutput> {
        if self.interrupt.is_set() {
            return Err(super::cancelled_before_start());
        }
        let result = self
            .inner
            .run_within_cancellable(program, args, timeout, &|| self.interrupt.is_set());
        match result {
            Ok(output) if output.success && self.interrupt.is_set() => {
                Err(super::cancelled_after_exit())
            }
            result => result,
        }
    }
    fn run_cleanup(
        &self,
        program: &Path,
        args: &[&OsStr],
    ) -> std::io::Result<super::CommandOutput> {
        self.inner.run(program, args)
    }
    fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.inner.find_program(name)
    }
    fn is_executable(&self, path: &Path) -> bool {
        self.inner.is_executable(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        self.inner.canonicalize(path)
    }
    fn dir_writable(&self, dir: &Path) -> bool {
        self.inner.dir_writable(dir)
    }
    fn home(&self) -> Option<PathBuf> {
        self.inner.home()
    }
    fn is_wsl(&self) -> bool {
        self.inner.is_wsl()
    }
    fn cancelled(&self) -> bool {
        self.interrupt.is_set()
    }
}

/// The CLI's `SIGINT`/`SIGTERM` handler, installed while it upgrades a copy.
///
/// It owns the two dispositions process-wide for as long as it is installed,
/// and is not reentrant: one [`Forwarding`] at a time, which `install`
/// asserts. Only the standalone CLI's [`run`] installs it; it coordinates
/// with no other user of these signals.
#[cfg(unix)]
mod forward {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set by the handler. A handler can do little more than store to an
    /// atomic, so the flag is a static. Never cleared: an interruption ends
    /// the run.
    static RECEIVED: AtomicBool = AtomicBool::new(false);

    /// Whether a [`Forwarding`] is installed.
    static INSTALLED: AtomicBool = AtomicBool::new(false);

    /// Run while a [`Forwarding`] is dropped, after the last read of
    /// [`RECEIVED`] inside the operation and before the dispositions are put
    /// back: where a signal that must not be lost arrives.
    #[cfg(test)]
    pub(super) static BEFORE_RESTORE: std::sync::Mutex<Option<fn()>> = std::sync::Mutex::new(None);

    const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

    extern "C" fn on_signal(_: libc::c_int) {
        RECEIVED.store(true, Ordering::SeqCst);
    }

    /// Whether a signal arrived while a [`Forwarding`] was installed.
    pub(super) fn received() -> bool {
        RECEIVED.load(Ordering::SeqCst)
    }

    /// The handler, installed for as long as this lives; dropping it puts
    /// back the dispositions it replaced.
    pub(super) struct Forwarding {
        previous: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl Forwarding {
        pub(super) fn install() -> Self {
            assert!(
                !INSTALLED.swap(true, Ordering::SeqCst),
                "the CLI's signal handler is installed once at a time"
            );
            let mut previous = Vec::new();
            for signal in SIGNALS {
                // SAFETY: a zeroed sigaction is a valid empty one; the handler
                // only stores to an atomic, which is async-signal-safe, and
                // `old` receives the disposition it replaces, restored on drop.
                unsafe {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                    action.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut action.sa_mask);
                    let mut old: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(signal, &action, &mut old) == 0 {
                        previous.push((signal, old));
                    }
                }
            }
            Self { previous }
        }
    }

    impl Drop for Forwarding {
        fn drop(&mut self) {
            #[cfg(test)]
            if let Some(hook) = BEFORE_RESTORE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                hook();
            }
            for (signal, old) in &self.previous {
                // SAFETY: `old` is the disposition sigaction(2) returned for
                // this signal when the handler was installed.
                unsafe {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
            }
            INSTALLED.store(false, Ordering::SeqCst);
        }
    }
}

/// Run the subcommand. Returns whether everything it attempted succeeded.
///
/// Detection, planning's probes and the upgrade itself wait on subprocesses
/// and the filesystem, so they run on a blocking thread
/// ([`tokio::task::spawn_blocking`]), never on the runtime driving the
/// downloads; the downloads inside an upgrade are driven from there through
/// the runtime's handle, as the TUI does.
///
/// The user's interruption is caught from the moment a copy's upgrade is
/// confirmed to its outcome ([`upgrade_each`]); then it says [`CANCELLED`]
/// and stops, attempting nothing further.
pub async fn run(
    host: Arc<dyn Host>,
    source: &ReleaseSource,
    options: &PlanOptions,
    args: Args,
    answers: Answers<'_>,
    interrupt: &Interrupt,
    out: &mut dyn Write,
) -> bool {
    let found = {
        let host = host.clone();
        tokio::task::spawn_blocking(move || {
            let running = detect::running(&*host, CopyKind::Cli)?;
            let other = match discover::other_copy(&*host, &running, None) {
                OtherCopy::Found(other) => Some(*other),
                OtherCopy::NotFound | OtherCopy::NotOffered => None,
            };
            Ok::<_, UpgradeError>((running, other))
        })
        .await
        .unwrap_or_else(|e| Err(UpgradeError::Io(e.to_string())))
    };
    let (running, other) = match found {
        Ok(found) => found,
        Err(e) => {
            say(out, &e.to_string());
            return false;
        }
    };
    let releases = match source.releases_for(&running, other.as_ref()).await {
        Ok(releases) => releases,
        Err(e) => {
            say(out, &e.to_string());
            return false;
        }
    };
    let plans: Vec<UpgradePlan> = std::iter::once(&running)
        .chain(other.as_ref())
        .map(|copy| plan::plan(copy, &releases, options))
        .collect();

    for (i, plan) in plans.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(out);
        }
        say_items(out, &plan.items());
    }
    if args.check {
        return true;
    }
    upgrade_each(
        host,
        &plans,
        source,
        &options.staging_root,
        args,
        answers,
        interrupt,
        out,
    )
    .await
}

/// Upgrade each actionable plan the user confirms, in order.
///
/// The `SIGINT`/`SIGTERM` handler is installed once a copy is confirmed and
/// kept until that copy's outcome, then the previous dispositions are put
/// back. So between copies and at a `[y/N]` question the signals keep them,
/// and `^C` there still ends the CLI. `interrupt` is read only after the
/// handler is gone, so a signal that arrived while it was being removed is
/// not lost; once it is set, this says [`CANCELLED`] and attempts no further
/// copy.
#[allow(clippy::too_many_arguments)]
async fn upgrade_each(
    host: Arc<dyn Host>,
    plans: &[UpgradePlan],
    source: &ReleaseSource,
    staging_root: &Path,
    args: Args,
    mut answers: Answers<'_>,
    interrupt: &Interrupt,
    out: &mut dyn Write,
) -> bool {
    let mut ok = true;
    for plan in plans.iter().filter(|plan| plan.is_actionable()) {
        if !confirmed(plan, args, &mut answers, out) {
            continue;
        }
        let result = {
            let _listening = interrupt.listen();
            execute_blocking(
                host.clone(),
                plan.clone(),
                source.clone(),
                staging_root,
                interrupt.clone(),
            )
            .await
        };
        match result {
            Ok(outcome) => {
                say_items(out, &outcome.items());
                ok &= outcome.upgraded();
            }
            Err(e) => {
                say(out, &e.to_string());
                say_items(out, &e.fallback());
                ok = false;
            }
        }
        if interrupt.is_set() {
            say(out, CANCELLED);
            return false;
        }
    }
    ok
}

/// [`execute::execute`] on a blocking thread: its subprocesses and file work
/// block there, and its downloads are driven through the current runtime's
/// handle. Once `interrupt` is set while it waits on a download, the upgrade
/// is dropped there, before the installed copy is changed
/// ([`UpgradeError::Cancelled`]); its private staging directory is then
/// removed on a best-effort basis. The `select!` cannot interrupt the
/// synchronous work after the downloads, so that work sees the interruption
/// through the host: its commands are refused, and [`Host::cancelled`] stops
/// it right before it would rename the new copy into place.
async fn execute_blocking(
    host: Arc<dyn Host>,
    plan: UpgradePlan,
    source: ReleaseSource,
    staging_root: &std::path::Path,
    interrupt: Interrupt,
) -> Result<execute::Outcome, UpgradeError> {
    let handle = tokio::runtime::Handle::current();
    let staging_root = staging_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        handle.block_on(async {
            tokio::select! {
                biased;
                result = execute::execute(&*host, &plan, &source, &staging_root) => result,
                () = interrupt.until_set() => Err(UpgradeError::Cancelled),
            }
        })
    })
    .await
    .unwrap_or_else(|e| Err(UpgradeError::Io(e.to_string())))
}

fn confirmed(
    plan: &UpgradePlan,
    args: Args,
    answers: &mut Answers<'_>,
    out: &mut dyn Write,
) -> bool {
    let Some(question) = plan.confirm_question() else {
        return false;
    };
    let question = crate::untrusted_text::strip_control_and_bidi(&question, false);
    let _ = writeln!(out);
    if args.yes {
        say(out, &format!("{question} yes (--yes)"));
        return true;
    }
    let Answers::Terminal(input) = answers else {
        say(
            out,
            &format!(
                "{question} Not asked, because this is not a terminal. Run `{} --yes` to upgrade without being asked.",
                super::UPGRADE_COMMAND
            ),
        );
        return false;
    };
    let _ = write!(out, "{question} [y/N] ");
    let _ = out.flush();
    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return false;
    }
    let yes = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !yes {
        let _ = writeln!(out, "Skipped.");
    }
    yes
}

/// The subcommand as `main` runs it: the real machine, this build's release
/// source, a terminal client's options (which ask `gh` whether provenance can
/// be checked, before any plan is printed), and stdin when it is a terminal.
pub fn main(args: Args) -> std::process::ExitCode {
    use std::io::IsTerminal;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let interrupt = Interrupt::from_signals();
    let host = Arc::new(CliHost {
        inner: super::SystemHost::default(),
        interrupt: interrupt.clone(),
    });
    let source = ReleaseSource::from_build();
    let options = PlanOptions::terminal(&*host);
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let answers = if std::io::stdin().is_terminal() {
        Answers::Terminal(&mut stdin)
    } else {
        Answers::None
    };
    let mut stdout = std::io::stdout();
    let ok = runtime.block_on(run(
        host,
        &source,
        &options,
        args,
        answers,
        &interrupt,
        &mut stdout,
    ));
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::{InstallMethod, Installation, Platform, Tools};
    use std::path::PathBuf;

    fn actionable() -> UpgradePlan {
        let exe = PathBuf::from("/home/u/.local/bin/dot-agent-deck");
        let installation = Installation {
            copy: CopyKind::Cli,
            executable: exe.clone(),
            version: "0.45.0".into(),
            platform: Some(Platform::LinuxAmd64),
            method: InstallMethod::DownloadedWritable { binary: exe },
            tools: Tools::default(),
        };
        plan::plan(
            &installation,
            &"0.46.0".into(),
            &PlanOptions {
                staging_root: PathBuf::from("/stage"),
                can_prompt_for_privilege: false,
                provenance: crate::self_upgrade::ProvenanceCheck::Unavailable {
                    reason: crate::self_upgrade::verify::GH_NOT_INSTALLED.into(),
                },
            },
        )
    }

    fn ask(args: Args, input: Option<&str>) -> (bool, String) {
        let mut out = Vec::new();
        let mut reader = input.map(|text| std::io::Cursor::new(text.as_bytes().to_vec()));
        let mut answers = match reader.as_mut() {
            Some(reader) => Answers::Terminal(reader),
            None => Answers::None,
        };
        let yes = confirmed(&actionable(), args, &mut answers, &mut out);
        (yes, String::from_utf8(out).unwrap())
    }

    #[test]
    fn cli_001_yes_flag_confirms_without_asking() {
        let (yes, out) = ask(
            Args {
                check: false,
                yes: true,
            },
            None,
        );
        assert!(yes);
        assert!(
            out.contains("Upgrade dot-agent-deck to v0.46.0? yes (--yes)"),
            "{out}"
        );
    }

    #[test]
    fn cli_002_terminal_answer_decides_and_default_is_no() {
        assert!(ask(Args::default(), Some("y\n")).0);
        assert!(ask(Args::default(), Some("YES\n")).0);
        let (yes, out) = ask(Args::default(), Some("\n"));
        assert!(!yes);
        assert!(out.contains("[y/N] Skipped."), "{out}");
    }

    #[test]
    fn cli_003_no_terminal_never_upgrades_unasked() {
        let (yes, out) = ask(Args::default(), None);
        assert!(!yes);
        assert!(out.contains("dot-agent-deck upgrade --yes"), "{out}");
    }

    // Unix paths the fake host answers for: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn cli_004_the_upgrade_runs_off_the_runtime_thread() {
        use crate::self_upgrade::HomebrewFormula;
        use crate::self_upgrade::test_host::{FakeHost, ok};
        use std::sync::Mutex;

        let ran_on = Arc::new(Mutex::new(None));
        let seen = ran_on.clone();
        let host: Arc<dyn Host> = Arc::new(
            FakeHost::new()
                .exe("/opt/homebrew/bin/brew")
                .handle("/opt/homebrew/bin/brew", move |_| {
                    *seen.lock().unwrap() = Some(std::thread::current().id());
                    ok("")
                })
                .deck("/opt/homebrew/bin/dot-agent-deck", "0.46.0"),
        );
        let mut installation = actionable().installation;
        installation.method = InstallMethod::Homebrew {
            formula: HomebrewFormula::Stable,
            prefix: PathBuf::from("/opt/homebrew"),
        };
        installation.tools.brew = Some(PathBuf::from("/opt/homebrew/bin/brew"));
        let plan = plan::plan(
            &installation,
            &"0.46.0".into(),
            &PlanOptions {
                staging_root: PathBuf::from("/stage"),
                can_prompt_for_privilege: false,
                provenance: crate::self_upgrade::ProvenanceCheck::Unavailable {
                    reason: "x".into(),
                },
            },
        );
        let source = ReleaseSource {
            api_url: String::new(),
            list_url: String::new(),
            download_base: String::new(),
        };
        // The CLI's own runtime: one thread, the caller's.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime
            .block_on(execute_blocking(
                host,
                plan,
                source,
                std::path::Path::new("/stage"),
                Interrupt::default(),
            ))
            .unwrap();
        assert!(outcome.upgraded(), "{outcome:?}");
        let ran_on = ran_on.lock().unwrap().expect("brew ran");
        assert_ne!(
            ran_on,
            std::thread::current().id(),
            "the upgrade's subprocess ran on the runtime's thread"
        );
    }

    #[test]
    fn cli_005_what_the_cli_prints_is_filtered_and_commands_are_unchanged() {
        let mut plan = actionable();
        let hostile = PathBuf::from("/home/u/\u{1b}[2J\u{202E}evil/dot-agent-deck");
        plan.installation.executable = hostile.clone();
        plan.action = crate::self_upgrade::PlanAction::ReplaceBinary {
            target: hostile,
            asset: "dot-agent-deck-linux-amd64".into(),
        };
        let mut out = Vec::new();
        for line in plan.lines() {
            say(&mut out, &line);
        }
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("/home/u/[2Jevil/dot-agent-deck"),
            "{printed}"
        );
        assert!(
            !printed.contains('\u{1b}') && !printed.contains('\u{202E}'),
            "{printed:?}"
        );

        let command = "echo 'abc  /s/x' | sha256sum -c - && sudo install -m 0755 /s/x /usr/local/bin/dot-agent-deck";
        let outcome = execute::Outcome::Staged {
            path: PathBuf::from("/s/x"),
            command: Some(command.into()),
            version: "0.46.0".into(),
            provenance: crate::self_upgrade::Provenance::Verified,
        };
        let mut out = Vec::new();
        for line in outcome.lines() {
            say(&mut out, &line);
        }
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains(&format!("  {command}\n")), "{printed}");

        let error = UpgradeError::CommandFailed {
            command: "brew upgrade dot-agent-deck".into(),
            detail: "\u{1b}]0;owned\u{7}Error".into(),
        };
        let mut out = Vec::new();
        say(&mut out, &error.to_string());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "`brew upgrade dot-agent-deck` failed: ]0;ownedError\n"
        );
    }

    /// Scenario: the CLI prints a result whose lines include commands. One
    /// the core built prints exactly as built, and one its output filter
    /// would change (a bidi override in a path) is not printed in any form,
    /// rewritten or not: a line says a command was left out instead.
    #[test]
    fn cli_006_a_command_the_filter_would_change_is_refused_not_rewritten() {
        let faithful = r"/usr/bin/hdiutil detach -force '/s/it'\''s here/mount'";
        let hostile = "/usr/bin/hdiutil detach -force '/s/mo\u{202E}unt'";
        let mut out = Vec::new();
        say_items(
            &mut out,
            &[
                plan::PlanLine::Text("Detach it with:".into()),
                plan::PlanLine::Command(hostile.into()),
                plan::PlanLine::Command(faithful.into()),
            ],
        );
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(
            printed,
            format!("Detach it with:\n{COMMAND_NOT_SHOWN}\n  {faithful}\n")
        );
        assert!(!printed.contains("/s/mount"), "{printed}");
    }

    /// Scenario: once the user interrupted the run, the CLI's host starts no
    /// command: running one returns "cancelled" at once and the command never
    /// runs. A cleanup command (the disk-image detach, the `.deb` state reads)
    /// still runs to its end.
    #[cfg(unix)]
    #[test]
    fn cli_007_an_interrupted_host_starts_no_command_but_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let interrupt = Interrupt::default();
        interrupt.set();
        let host = CliHost {
            inner: super::super::SystemHost::default(),
            interrupt,
        };
        let sh = Path::new("/bin/sh");
        let ran = dir.path().join("ran");
        let script = format!("echo ran > '{}'", ran.display());
        let err = host
            .run(sh, &[OsStr::new("-c"), OsStr::new(&script)])
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "{err:?}");
        assert_eq!(
            err.to_string(),
            "it was not started, because the upgrade was cancelled"
        );
        assert!(super::super::was_cancelled(&err));
        assert!(!ran.exists(), "a command started after the interruption");

        let cleaned = dir.path().join("cleaned");
        let script = format!("echo cleaned > '{}'", cleaned.display());
        let output = host
            .run_cleanup(sh, &[OsStr::new("-c"), OsStr::new(&script)])
            .expect("a cleanup command runs after the interruption");
        assert!(output.success, "{output:?}");
        assert!(cleaned.exists(), "the cleanup command did not run");
    }

    /// Scenario: a command exits successfully but leaves a descendant holding
    /// its output open for a second, and the user interrupts the run in that
    /// second, while the output is still read. The call reports the command
    /// as cancelled, not as its success, so the upgrade never acts on it.
    #[cfg(unix)]
    #[test]
    fn cli_008_a_command_interrupted_after_it_exited_is_not_a_success() {
        let dir = tempfile::tempdir().unwrap();
        let exited = dir.path().join("exited");
        let script = format!(
            "echo 'dot-agent-deck 0.46.0'; /bin/sleep 1 & echo $$ > '{}'; exit 0",
            exited.display()
        );
        let interrupt = Interrupt::default();
        let host = CliHost {
            inner: super::super::SystemHost::default(),
            interrupt: interrupt.clone(),
        };
        let canceller = {
            let exited = exited.clone();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while !std::fs::read_to_string(&exited).is_ok_and(|s| s.ends_with('\n')) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the command never ran"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // The command has exited; its descendant holds the output
                // open for about a second more.
                std::thread::sleep(std::time::Duration::from_millis(300));
                interrupt.set();
            })
        };
        let result = host.run_within(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new(&script)],
            std::time::Duration::from_secs(30),
        );
        canceller.join().unwrap();
        let err = result.expect_err("a run interrupted after the command exited succeeded");
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted, "{err:?}");
        assert_eq!(
            err.to_string(),
            "it finished, but the upgrade was cancelled before its result was used"
        );
        assert_eq!(super::super::unfinished_stopped(&err), Some(true));
    }

    /// A Homebrew copy whose `brew` is at `<prefix>/bin/brew`, offered 0.46.0.
    #[cfg(unix)]
    fn brew_plan(prefix: &str) -> UpgradePlan {
        let mut installation = actionable().installation;
        installation.executable = PathBuf::from(format!("{prefix}/bin/dot-agent-deck"));
        installation.method = InstallMethod::Homebrew {
            formula: crate::self_upgrade::HomebrewFormula::Stable,
            prefix: PathBuf::from(prefix),
        };
        installation.tools.brew = Some(PathBuf::from(format!("{prefix}/bin/brew")));
        plan::plan(
            &installation,
            &"0.46.0".into(),
            &PlanOptions {
                staging_root: PathBuf::from("/stage"),
                can_prompt_for_privilege: false,
                provenance: crate::self_upgrade::ProvenanceCheck::Unavailable {
                    reason: "x".into(),
                },
            },
        )
    }

    /// Scenario: `upgrade --yes` with two copies to upgrade. A `SIGINT`
    /// arrives once the first copy's upgrade is over, while the CLI puts the
    /// previous signal dispositions back (a test-only seam raises it there).
    /// The signal is not lost: the CLI prints the first copy's outcome, then
    /// `Cancelled.`, reports failure, and never asks about or starts the
    /// second copy. Afterwards `SIGINT` is back at its default.
    #[cfg(unix)]
    #[test]
    fn cli_009_a_signal_while_the_handler_is_removed_is_not_lost() {
        use crate::self_upgrade::test_host::{FakeHost, ok};
        let path =
            "self_upgrade::cli::tests::cli_009_a_signal_while_the_handler_is_removed_is_not_lost";
        if !crate::self_upgrade::tests::is_reexec_child() {
            crate::self_upgrade::tests::reexec(path);
            return;
        }
        let host = Arc::new(
            FakeHost::new()
                .exe("/opt/homebrew/bin/brew")
                .exe("/usr/local/bin/brew")
                .handle("/opt/homebrew/bin/brew", |_| ok(""))
                .handle("/usr/local/bin/brew", |_| ok(""))
                .deck("/opt/homebrew/bin/dot-agent-deck", "0.46.0")
                .deck("/usr/local/bin/dot-agent-deck", "0.46.0"),
        );
        let plans = [brew_plan("/opt/homebrew"), brew_plan("/usr/local")];
        // SAFETY: raise(3) delivers SIGINT to this thread before it returns;
        // the hook runs while the CLI's handler is still installed, so the
        // signal only sets its flag. This process is the re-exec'd half
        // running this one test, so nothing else sees the signal.
        *forward::BEFORE_RESTORE.lock().unwrap() = Some(|| unsafe {
            libc::raise(libc::SIGINT);
        });
        let source = ReleaseSource {
            api_url: String::new(),
            list_url: String::new(),
            download_base: String::new(),
        };
        let interrupt = Interrupt::from_signals();
        let mut out = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ok = runtime.block_on(upgrade_each(
            host.clone(),
            &plans,
            &source,
            Path::new("/stage"),
            Args {
                check: false,
                yes: true,
            },
            Answers::None,
            &interrupt,
            &mut out,
        ));
        let printed = String::from_utf8(out).unwrap();
        assert!(
            forward::BEFORE_RESTORE.lock().unwrap().is_none(),
            "the seam never ran"
        );
        assert!(!ok, "{printed}");
        assert!(printed.ends_with("Cancelled.\n"), "{printed}");
        assert_eq!(printed.matches("yes (--yes)").count(), 1, "{printed}");
        let ran = host.ran();
        assert!(
            ran.contains(&"/opt/homebrew/bin/brew upgrade dot-agent-deck".to_string()),
            "{ran:?}"
        );
        assert!(
            !ran.iter().any(|line| line.starts_with("/usr/local/")),
            "the second copy was attempted: {ran:?}"
        );
        // SAFETY: a null new action only reads the current disposition.
        let disposition = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current);
            current.sa_sigaction
        };
        assert_eq!(disposition, libc::SIG_DFL, "SIGINT was not put back");
    }
}
