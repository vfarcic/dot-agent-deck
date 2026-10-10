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

/// What the CLI prints when the user interrupted it while it ran a command.
pub const CANCELLED: &str = "Cancelled.";

/// The CLI's [`Host`]: [`super::SystemHost`], with a Ctrl+C (`SIGINT`) or
/// `SIGTERM` the CLI receives while it runs a command forwarded to that
/// command as a cancellation ([`super::SystemHost::run_within_cancellable`]).
///
/// The command runs in a session of its own, so the terminal's `^C` reaches
/// only the CLI; without this, the CLI would die and leave the command
/// running with nothing left to bound it. While a command runs, the two
/// signals only set a flag; the run then stops the command's group, waits up
/// to its grace for it to exit, and says it was cancelled. Between commands
/// the signals keep their dispositions, so `^C` at a question still ends the
/// CLI. `interrupted` stays set once a command was cancelled, which
/// [`run`] reads to stop.
pub struct CliHost {
    pub inner: super::SystemHost,
    pub interrupted: Arc<AtomicBool>,
}

impl Host for CliHost {
    fn run_within(
        &self,
        program: &Path,
        args: &[&OsStr],
        timeout: std::time::Duration,
    ) -> std::io::Result<super::CommandOutput> {
        #[cfg(unix)]
        {
            let forwarding = forward::Forwarding::install();
            let result = self
                .inner
                .run_within_cancellable(program, args, timeout, &|| forward::received());
            if forward::received() {
                self.interrupted.store(true, Ordering::SeqCst);
            }
            drop(forwarding);
            result
        }
        #[cfg(not(unix))]
        {
            self.inner.run_within(program, args, timeout)
        }
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
}

/// The CLI's `SIGINT`/`SIGTERM` handling while it runs a command.
#[cfg(unix)]
mod forward {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set by the handler: a signal arrived while a command ran. A handler
    /// can do little more than store to an atomic, so the flag is a static.
    static RECEIVED: AtomicBool = AtomicBool::new(false);

    /// Held while the handler is installed, so two runs never interleave
    /// their saving and restoring of the dispositions.
    static INSTALLED: Mutex<()> = Mutex::new(());

    const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

    extern "C" fn on_signal(_: libc::c_int) {
        RECEIVED.store(true, Ordering::SeqCst);
    }

    /// Whether a signal arrived since the current [`Forwarding`] was
    /// installed.
    pub(super) fn received() -> bool {
        RECEIVED.load(Ordering::SeqCst)
    }

    /// The handler, installed for as long as this lives; dropping it puts
    /// back the dispositions it replaced.
    pub(super) struct Forwarding {
        previous: Vec<(libc::c_int, libc::sigaction)>,
        _installed: std::sync::MutexGuard<'static, ()>,
    }

    impl Forwarding {
        pub(super) fn install() -> Self {
            let installed = INSTALLED.lock().unwrap_or_else(|e| e.into_inner());
            RECEIVED.store(false, Ordering::SeqCst);
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
            Self {
                previous,
                _installed: installed,
            }
        }
    }

    impl Drop for Forwarding {
        fn drop(&mut self) {
            for (signal, old) in &self.previous {
                // SAFETY: `old` is the disposition sigaction(2) returned for
                // this signal when the handler was installed.
                unsafe {
                    libc::sigaction(*signal, old, std::ptr::null_mut());
                }
            }
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
/// Once `interrupted` is set (a command was cancelled, [`CliHost`]), it says
/// [`CANCELLED`] and stops, attempting nothing further.
pub async fn run(
    host: Arc<dyn Host>,
    source: &ReleaseSource,
    options: &PlanOptions,
    args: Args,
    mut answers: Answers<'_>,
    interrupted: &AtomicBool,
    out: &mut dyn Write,
) -> bool {
    let stop_if_interrupted = |out: &mut dyn Write| {
        let stop = interrupted.load(Ordering::SeqCst);
        if stop {
            say(out, CANCELLED);
        }
        stop
    };
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
    if stop_if_interrupted(out) {
        return false;
    }
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

    let mut ok = true;
    for plan in plans.iter().filter(|plan| plan.is_actionable()) {
        if !confirmed(plan, args, &mut answers, out) {
            continue;
        }
        match execute_blocking(
            host.clone(),
            plan.clone(),
            source.clone(),
            &options.staging_root,
        )
        .await
        {
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
        if stop_if_interrupted(out) {
            return false;
        }
    }
    ok
}

/// [`execute::execute`] on a blocking thread: its subprocesses and file work
/// block there, and its downloads are driven through the current runtime's
/// handle.
async fn execute_blocking(
    host: Arc<dyn Host>,
    plan: UpgradePlan,
    source: ReleaseSource,
    staging_root: &std::path::Path,
) -> Result<execute::Outcome, UpgradeError> {
    let handle = tokio::runtime::Handle::current();
    let staging_root = staging_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        handle.block_on(execute::execute(&*host, &plan, &source, &staging_root))
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
    let interrupted = Arc::new(AtomicBool::new(false));
    let host = Arc::new(CliHost {
        inner: super::SystemHost::default(),
        interrupted: interrupted.clone(),
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
        &interrupted,
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
}
