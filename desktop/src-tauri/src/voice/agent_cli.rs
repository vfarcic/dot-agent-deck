//! The zero-configuration intent backend: spawn the agent CLI the user already
//! has.
//!
//! This is PRD #802's whole answer to *a feature that needs credentials before
//! it does anything is a feature most people never try*. The app asks for
//! nothing: `claude` and `opencode` are already authenticated on the machine,
//! with whatever the **user** gave them, so the first utterance works on the
//! day the app is installed.
//!
//! **"No key" is narrower than it sounds and the narrow version is still the
//! interesting one.** A credential exists — the CLI's — and it is spent per
//! utterance. What is true is that the *app* needs none and asks for none.
//!
//! # Built against what was measured, not against what was promised
//!
//! Measured on 2026-09-18 with `claude 2.1.277` and the prompt
//! [`super::prompt::cli_prompt`] actually sends:
//!
//! | | `claude -p --model claude-haiku-4-5` | `opencode run --pure` |
//! | --- | --- | --- |
//! | wall clock | 3.08 s, 3.51 s | 4.43 s |
//! | output | ```` ```json ````-fenced, **both runs** | bare object, no fence |
//! | answer | `{"action":"open_agent","params":{"agent":"tester"}}` | identical |
//!
//! Those are the CLI's own wall clock with a short prompt. **Driven through
//! the whole pipeline with the shipping prompt — the annotated command list and
//! the agent labels included — the same backend measured 4.30 s and 6.30 s**,
//! and the larger figure is the honest one to quote at a user: a bigger prompt
//! is what this code actually sends. PRD #802's own survey measured 4.49–4.68 s
//! for `claude` with `--output-format json`, which this does not ask for.
//!
//! Either way it is **slow**, and the PRD says so rather than pretending
//! otherwise. The keyed [`super::remote`] backend is the same milestone's
//! answer — 0.62–1.03 s through that same pipeline — and it is what a user who
//! minds the wait switches to.
//!
//! So: the fence is expected rather than tolerated, a banner on stdout is
//! expected ([`super::prompt::extract_answer`] has the measurement), and
//! **every** failure is an `Err`, which [`super::handle_utterance`] renders as
//! a sentence rather than a hang.
//!
//! # The five properties copied from `codex_hooks_manage::list_hooks_in`
//!
//! `src/codex_hooks_manage.rs:640` is this repo's one existing non-interactive
//! agent-CLI spawn, and PRD #802 names it as the shape to copy: a pinned child
//! environment, stderr discarded, a bounded wait behind a named constant, the
//! child killed before returning, and every failure mode an `Err`. All five are
//! here. The one deliberate divergence is the environment: that function pins
//! `CODEX_HOME` and this one **inherits**, because the CLI's own
//! authentication is the entire point and clearing it would break the property
//! this backend exists for. `PATH` is the only variable this touches, and only
//! when it has to — see [`AgentCliResolver::resolve`].

use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::OnceCell;

use super::prompt::{cli_prompt, extract_answer};
use super::resolver::{IntentError, IntentRequest, IntentResolver, ResolveFuture};

/// How long a spawned agent CLI gets before the attempt is abandoned.
///
/// **A backstop against a hang, not a latency budget.** The measurements above
/// are 3.1–4.7 s, so this is roughly four times the slowest of them; a call
/// that is merely slow should produce an answer rather than a failure sentence,
/// because the user has already waited and a timeout at that point spends the
/// wait and returns nothing. What it bounds is the genuinely stuck case — a CLI
/// waiting on a login prompt it will never get, a network that black-holes —
/// where every second past the first few is wasted.
///
/// M6 renders the latency the user actually paid ([`super::VoiceResult`]), so
/// a slow-but-working backend is visible rather than merely felt.
pub const AGENT_CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// The model the `claude` adapter pins.
///
/// The cheap fast tier, deliberately: this is short, closed-set, structured
/// output with no reasoning to do, which is the workload that tier is for, and
/// it is the same model PRD #802's survey measured and that
/// `docs/develop/config-gen-regeneration.md:49` already documents an
/// invocation of. Pinning it also stops a user's own default model — which may
/// be an expensive one — deciding what an utterance costs.
///
/// **`opencode` gets no equivalent pin**, and that is not an omission. Its
/// `-m` flag takes `provider/model`, so a pin would have to name a provider
/// this app cannot know the user has configured, and naming the wrong one
/// fails the call outright. Taking the user's own default is the best-effort
/// PRD #802 describes for that CLI.
const CLAUDE_MODEL: &str = "claude-haiku-4-5";

/// Which CLI an [`AgentCliResolver`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// `claude -p`, print mode. The default backend and the reference one for
    /// PRD #802 M9's phrase fixtures.
    Claude,
    /// `opencode run`. Best-effort — PRD #802 says so rather than claiming a
    /// parity it has not measured, though the one run above did work.
    Opencode,
}

impl AgentCli {
    /// The program name, which is also what PATH resolution looks for.
    pub fn program(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Opencode => "opencode",
        }
    }

    /// What [`super::VoiceResult`] names as the backend that answered.
    pub fn backend_name(self) -> &'static str {
        self.program()
    }

    /// The argv after the program, with the prompt as ONE element.
    ///
    /// One element and not several: there is no shell here, so an utterance
    /// containing quotes, `$`, backticks or newlines is passed through by the
    /// kernel verbatim and cannot become syntax. That is a property of
    /// `Command`, not of any escaping this file does — which is why this file
    /// does none.
    fn args(self, prompt: String) -> Vec<OsString> {
        match self {
            AgentCli::Claude => vec![
                OsString::from("-p"),
                OsString::from("--model"),
                OsString::from(CLAUDE_MODEL),
                OsString::from(prompt),
            ],
            // `--pure` runs without external plugins. A plugin is the most
            // likely source of the banner noise on stdout that the parser
            // already tolerates, and a one-shot classification wants none of
            // what a plugin offers.
            AgentCli::Opencode => vec![
                OsString::from("run"),
                OsString::from("--pure"),
                OsString::from(prompt),
            ],
        }
    }
}

/// Whether this resolver may go looking for the user's login-shell PATH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Spawn on the PATH this process inherited and nothing else. What the
    /// tests use, so no test can spend ten seconds in somebody's `~/.zshrc`.
    Inherit,
    /// Fall back to the user's login-shell PATH when the program is not found
    /// on the inherited one. The shipping behaviour — see
    /// [`AgentCliResolver::resolve`] for why it is a fallback and not a
    /// startup step.
    LoginShellFallback,
}

/// The user's login-shell PATH, captured at most once per process.
///
/// A [`OnceCell`] rather than a `OnceLock` because the capture is blocking and
/// has to be awaited: `get_or_init` serialises concurrent first utterances onto
/// one capture instead of racing several interactive shells.
///
/// **A failed capture is remembered as a failure.** A machine with no `$SHELL`,
/// or whose shell cannot be run, should not pay [`CAPTURE_TIMEOUT`] again on
/// every utterance to learn the same thing.
///
/// [`CAPTURE_TIMEOUT`]: dot_agent_deck::login_shell::CAPTURE_TIMEOUT
static LOGIN_SHELL_PATH: OnceCell<Option<String>> = OnceCell::const_new();

/// PRD #802 Open Question 1, answered: the login-shell PATH is captured
/// **lazily, at most once, and only when the program was not found without
/// it**.
///
/// The measurement and the reasoning are in the PRD's Open Questions section.
/// The short version is three facts:
///
/// - **`apply_login_shell_path()` is the wrong half of that API here.** It
///   calls `std::env::set_var`, and its own contract says exactly once, at
///   process startup, before any runtime or thread exists. A desktop app
///   resolves an utterance on a multi-threaded Tauri runtime, long outside that
///   window, where the same call would be a `getenv`/`setenv` data race. So
///   this uses the **capture** half, which mutates nothing, and puts the result
///   on the child's own environment.
/// - **The cost is real and wildly variable.** Measured at **0.04 s** for this
///   box's `bash`, against the **~6 s** `zsh -ilc` that `CAPTURE_TIMEOUT`'s own
///   doc comment records and a **10 s** ceiling. Spending an unknown fraction
///   of that on every app start, for every user, including everyone who never
///   uses voice, buys nothing.
/// - **The common case needs none of it.** An app launched from a terminal
///   already has the PATH it needs. Only the case Open Question 1 actually asks
///   about — Finder, a desktop launcher — pays, and it pays once.
async fn login_shell_path() -> Option<String> {
    LOGIN_SHELL_PATH
        .get_or_init(|| async {
            // Blocking by nature: it runs an interactive login shell and polls
            // for up to CAPTURE_TIMEOUT. Doing that on a runtime thread is how
            // an app stops repainting.
            tokio::task::spawn_blocking(dot_agent_deck::login_shell::capture_login_shell_path)
                .await
                .unwrap_or_default()
        })
        .await
        .clone()
}

/// Resolve intent by asking an agent CLI already installed on this machine.
pub struct AgentCliResolver {
    cli: AgentCli,
    /// What to spawn. The CLI's own name in production; a stub script under a
    /// `tempfile::tempdir()` in the tests, which is how the timeout, the
    /// fence, the banner and the malformed-output paths are all asserted
    /// without a credential and without spawning a real agent.
    program: OsString,
    timeout: Duration,
    path_mode: PathMode,
}

impl AgentCliResolver {
    /// The default backend: `claude -p`, no key of the app's own.
    pub fn claude() -> Self {
        Self::new(AgentCli::Claude)
    }

    /// The other CLI. Best-effort, per PRD #802.
    pub fn opencode() -> Self {
        Self::new(AgentCli::Opencode)
    }

    pub fn new(cli: AgentCli) -> Self {
        Self {
            cli,
            program: OsString::from(cli.program()),
            timeout: AGENT_CLI_TIMEOUT,
            path_mode: PathMode::LoginShellFallback,
        }
    }

    /// Spawn `program` instead of the CLI's own name, and never go looking for
    /// a login-shell PATH.
    ///
    /// Both halves are what makes this backend testable: the tests point it at
    /// a script they wrote, and a test must never spend `CAPTURE_TIMEOUT` in
    /// the developer's own shell profile.
    pub fn with_program(mut self, program: impl AsRef<OsStr>) -> Self {
        self.program = program.as_ref().to_os_string();
        self.path_mode = PathMode::Inherit;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The `Err` a failure of this backend becomes.
    ///
    /// [`IntentError::NotConfigured`] only for "the CLI is not installed",
    /// because that is the one failure whose remedy is a settings or
    /// installation instruction rather than a retry. Everything else — a
    /// non-zero exit, a timeout, unreadable output — is
    /// [`IntentError::Backend`].
    fn failed(&self, detail: impl AsRef<str>) -> IntentError {
        IntentError::Backend(format!("{} {}", self.cli.program(), detail.as_ref().trim()))
    }

    async fn run(&self, request: IntentRequest<'_>) -> Result<super::IntentAnswer, IntentError> {
        let prompt = cli_prompt(&request);
        let output = match self.spawn(&prompt, None).await {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // The Finder case, and the only one that pays for a capture.
                match self.recovery_path().await {
                    Some(path) => self.spawn(&prompt, Some(&path)).await,
                    None => Err(error),
                }
            }
            other => other,
        };

        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(IntentError::NotConfigured(format!(
                    "`{}` is not installed, or is not on this app's PATH",
                    self.cli.program()
                )));
            }
            Err(error) if error.kind() == ErrorKind::TimedOut => {
                return Err(
                    self.failed(format!("did not answer within {}s", self.timeout.as_secs()))
                );
            }
            Err(error) => return Err(self.failed(format!("could not be run ({error})"))),
        };

        // A non-zero exit is reported even when stdout held something
        // parseable: a CLI that failed and printed an answer anyway is not a
        // CLI whose answer should be dispatched.
        if !output.status.success() {
            return Err(self.failed(format!("exited with {}", output.status)));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        extract_answer(&stdout).map_err(|reason| self.failed(reason.reason()))
    }

    /// The login-shell PATH, when this resolver is allowed to go looking.
    async fn recovery_path(&self) -> Option<String> {
        match self.path_mode {
            PathMode::Inherit => None,
            PathMode::LoginShellFallback => login_shell_path().await,
        }
    }

    /// One bounded, killed-on-every-path spawn.
    ///
    /// A timeout is reported as [`ErrorKind::TimedOut`] so the one `match` in
    /// [`Self::run`] classifies every failure, rather than the timeout being a
    /// second control-flow shape beside the `io::Error`s.
    ///
    /// **The child is killed before this returns on the timeout path**, and by
    /// the mechanism rather than by a line that could be skipped:
    /// `kill_on_drop(true)` means dropping the `Child` signals it, and
    /// `tokio::time::timeout` drops the future it was given — and with it the
    /// `Child` that future owns — *before* yielding `Err`. Reaping the killed
    /// process is then tokio's background job, so this returns without waiting
    /// on it.
    async fn spawn(
        &self,
        prompt: &str,
        path: Option<&str>,
    ) -> Result<std::process::Output, std::io::Error> {
        let mut command = Command::new(&self.program);
        command
            .args(self.cli.args(prompt.to_string()))
            // Nothing is written to the child, and a CLI that decides to prompt
            // must hit EOF rather than wait on a terminal that is not there.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Discarded, exactly as the `codex app-server` precedent does:
            // stderr here is progress chatter and warnings, and the failure
            // wording this backend produces is its own.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = path {
            // The ONE variable this touches. Everything else is inherited,
            // because the CLI's own authentication — a credentials file under
            // `$HOME`, a keychain entry, an environment variable the user
            // exported — is the entire point of this backend.
            command.env("PATH", path);
        }

        let child = command.spawn()?;
        // `wait_with_output` drains stdout while it waits, so a CLI that prints
        // more than a pipe buffer before exiting cannot deadlock the wait.
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "the agent CLI did not answer in time",
            )),
        }
    }
}

impl IntentResolver for AgentCliResolver {
    fn resolve<'a>(&'a self, request: IntentRequest<'a>) -> ResolveFuture<'a> {
        Box::pin(self.run(request))
    }

    fn backend_name(&self) -> &'static str {
        self.cli.backend_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::schema::{AnnotatedCommand, annotate};
    use crate::voice::table::{Screen, table};
    use crate::voice::{IntentAnswer, Transcript};

    /// A stub executable, written into a `tempfile::tempdir()`.
    ///
    /// PRD #802 M5's test rule: nothing in the merge-blocking tier spawns a
    /// real agent or needs a credential. A script this test wrote is a real
    /// child process with real pipes, real exit codes and a real timeout — it
    /// exercises every path the production spawn takes, and it costs
    /// milliseconds.
    struct Stub {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl Stub {
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("stub-cli");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            Self { _dir: dir, path }
        }

        fn resolver(&self) -> AgentCliResolver {
            AgentCliResolver::claude().with_program(&self.path)
        }
    }

    fn commands() -> Vec<AnnotatedCommand> {
        annotate(table(), Screen::Deck)
    }

    fn request<'a>(
        transcript: &'a Transcript,
        commands: &'a [AnnotatedCommand],
    ) -> IntentRequest<'a> {
        IntentRequest {
            transcript,
            commands,
            agents: &[],
        }
    }

    async fn ask(resolver: &AgentCliResolver, said: &str) -> Result<IntentAnswer, IntentError> {
        let commands = commands();
        let transcript = Transcript::new(said);
        resolver.resolve(request(&transcript, &commands)).await
    }

    // -- the measured output shapes ----------------------------------------

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_parses_a_fence_wrapped_answer() {
        // The shape `claude -p` produced on EVERY measured run.
        let stub = Stub::new(
            "printf '```json\\n{\"action\":\"open_agent\",\"params\":{\"agent\":\"tester\"}}\\n```'",
        );
        let answer = ask(&stub.resolver(), "show me the tester")
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_agent");
        assert_eq!(answer.params.get("agent"), Some(&"tester".to_string()));
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_parses_plain_json() {
        // The shape `opencode run --pure` produced.
        let stub = Stub::new("printf '{\"action\":\"open_overview\"}'");
        let answer = ask(&stub.resolver(), "show me everything")
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_overview");
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_tolerates_a_leading_banner() {
        let stub = Stub::new(concat!(
            "echo 'Warning: ANTHROPIC_API_KEY takes precedence over your login.'\n",
            "printf '{\"action\":\"none\"}'"
        ));
        let answer = ask(&stub.resolver(), "what time is it")
            .await
            .expect("answers");
        assert!(answer.is_no_match());
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_ignores_stderr_chatter() {
        let stub = Stub::new(concat!(
            "echo 'loading plugins...' >&2\n",
            "printf '{\"action\":\"open_deck\"}'"
        ));
        let answer = ask(&stub.resolver(), "back to the deck")
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_deck");
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_passes_the_utterance_as_one_argv_element() {
        // No shell, so an utterance full of shell metacharacters arrives
        // verbatim and cannot become syntax. The stub echoes its last argument
        // back inside the answer's param to prove the whole prompt landed.
        let stub = Stub::new(
            "for arg in \"$@\"; do last=\"$arg\"; done\n\
             case \"$last\" in \n\
             *'`whoami`; rm -rf /'*) printf '{\"action\":\"open_deck\"}' ;;\n\
             *) printf '{\"action\":\"none\"}' ;;\n\
             esac",
        );
        let answer = ask(&stub.resolver(), "go `whoami`; rm -rf / back")
            .await
            .expect("answers");
        assert_eq!(
            answer.action, "open_deck",
            "the utterance did not arrive verbatim"
        );
    }

    // -- every failure is an Err -------------------------------------------

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_reports_malformed_output() {
        let stub = Stub::new("printf 'I think you want to open the tester.'");
        let error = ask(&stub.resolver(), "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::Backend(detail) if detail.contains("no answer this build could read")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_reports_empty_output() {
        let stub = Stub::new("exit 0");
        let error = ask(&stub.resolver(), "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::Backend(detail) if detail.contains("no output")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_reports_a_non_zero_exit_even_with_parseable_output() {
        // A CLI that failed and printed an answer anyway is not a CLI whose
        // answer should be dispatched.
        let stub = Stub::new("printf '{\"action\":\"open_deck\"}'\nexit 3");
        let error = ask(&stub.resolver(), "back to the deck")
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::Backend(detail) if detail.contains("exited with")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn voice_agent_cli_reports_a_missing_binary_as_not_configured() {
        // NotConfigured rather than Backend: the remedy is an installation
        // instruction, not a retry. `with_program` also pins PathMode::Inherit,
        // so this cannot wander into the developer's shell profile.
        let resolver = AgentCliResolver::claude()
            .with_program("/nonexistent/dot-agent-deck-voice-stub-does-not-exist");
        let error = ask(&resolver, "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::NotConfigured(detail) if detail.contains("not installed")),
            "got {error:?}"
        );
    }

    // -- the bounded wait --------------------------------------------------

    // `#[cfg(unix)]` and NOT the `cfg_attr(..., ignore)` its siblings carry, and
    // the difference is a compile error rather than a style choice: an `ignore`
    // skips a test at RUN time and still compiles its body, and this body names
    // `wait_for_exit`, which is `#[cfg(unix)]` because it reads `/proc`. Under
    // `scripts/windows-cross-check.sh` that was an E0425 in the lib test target
    // — which `build-windows` builds, since it runs `cargo nextest run
    // --workspace`. Caught by PRD #802 M7's own run of that script, whose
    // header says only errors matter; this was one.
    #[cfg(unix)]
    #[tokio::test]
    async fn voice_agent_cli_times_out_and_leaves_no_child_behind() {
        // The stub writes its own pid, then sleeps far past the timeout. After
        // the Err, that pid must be gone: `kill_on_drop` fires when `timeout`
        // drops the future holding the `Child`, which happens before the Err
        // reaches us.
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("pid");
        let stub = Stub::new(&format!(
            "echo $$ > '{}'\nsleep 120",
            pidfile.to_string_lossy()
        ));
        let resolver = stub.resolver().with_timeout(Duration::from_millis(250));

        let started = std::time::Instant::now();
        let error = ask(&resolver, "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the wait was not bounded: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(&error, IntentError::Backend(detail) if detail.contains("did not answer")),
            "got {error:?}"
        );

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the stub recorded its pid")
            .trim()
            .parse()
            .expect("a pid");
        // The reap is tokio's background job, so poll briefly rather than
        // asserting the corpse is already collected. What is asserted is that
        // the process is not still RUNNING.
        let gone = wait_for_exit(pid, Duration::from_secs(5));
        assert!(gone, "pid {pid} survived the timeout");
    }

    /// Whether `pid` has stopped running, within `budget`.
    ///
    /// Reads `/proc` rather than sending a signal: a killed child that has not
    /// been reaped yet is a zombie, and `kill(pid, 0)` succeeds for a zombie —
    /// which would make this assert nothing at all on the very path it exists
    /// to check.
    #[cfg(unix)]
    fn wait_for_exit(pid: i32, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            let running = match &state {
                // Field 3 is the state character; `Z` is a reaped-pending
                // corpse, which is dead for this test's purposes.
                Ok(stat) => !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z'),
                Err(_) => false,
            };
            if !running {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // -- identity ----------------------------------------------------------

    #[test]
    fn voice_agent_cli_names_itself_for_the_surface() {
        assert_eq!(AgentCliResolver::claude().backend_name(), "claude");
        assert_eq!(AgentCliResolver::opencode().backend_name(), "opencode");
    }

    #[test]
    fn voice_agent_cli_pins_the_cheap_fast_tier_for_claude_only() {
        let claude = AgentCli::Claude.args("SAID".to_string());
        assert_eq!(
            claude,
            vec![
                OsString::from("-p"),
                OsString::from("--model"),
                OsString::from("claude-haiku-4-5"),
                OsString::from("SAID"),
            ]
        );
        // `opencode`'s `-m` takes `provider/model`, and this app cannot know
        // which providers the user configured — so no pin, deliberately.
        let opencode = AgentCli::Opencode.args("SAID".to_string());
        assert_eq!(
            opencode,
            vec![
                OsString::from("run"),
                OsString::from("--pure"),
                OsString::from("SAID"),
            ]
        );
        assert!(!opencode.iter().any(|arg| arg == "-m"));
    }

    #[test]
    fn voice_agent_cli_defaults_to_the_login_shell_fallback_and_tests_do_not() {
        assert_eq!(
            AgentCliResolver::claude().path_mode,
            PathMode::LoginShellFallback
        );
        // The property every test above depends on: `with_program` also pins
        // Inherit, so no test can spend CAPTURE_TIMEOUT in a real shell.
        assert_eq!(
            AgentCliResolver::claude()
                .with_program("/bin/true")
                .path_mode,
            PathMode::Inherit
        );
    }

    #[tokio::test]
    async fn voice_agent_cli_inherit_mode_never_captures_a_login_shell() {
        // Asserted by the clock: a capture runs `$SHELL -ilc` and is bounded by
        // a 10s CAPTURE_TIMEOUT, so a miss that returns in milliseconds did not
        // make one.
        let resolver = AgentCliResolver::opencode()
            .with_program("/nonexistent/dot-agent-deck-voice-stub-does-not-exist");
        let started = std::time::Instant::now();
        let error = ask(&resolver, "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            matches!(error, IntentError::NotConfigured(_)),
            "got {error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a login shell was captured: {:?}",
            started.elapsed()
        );
    }
}
