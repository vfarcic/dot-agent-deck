//! The zero-configuration intent backend: spawn the agent CLI the user already
//! has.
//!
//! This is PRD #802's whole answer to *a feature that needs credentials before
//! it does anything is a feature most people never try*. The app asks for
//! nothing: `claude` is already authenticated on the machine, with whatever the
//! **user** gave it, so the first utterance works on the day the app is
//! installed.
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
//! | | `claude -p --model claude-haiku-4-5` |
//! | --- | --- |
//! | wall clock | 3.08 s, 3.51 s |
//! | output | ```` ```json ````-fenced, **both runs** |
//! | answer | `{"action":"open_agent","params":{"agent":"tester"}}` |
//!
//! Those are the CLI's own wall clock with a short prompt. **Driven through
//! the whole pipeline with the shipping prompt — the annotated command list and
//! the agent labels included — the same backend measured 4.30 s**, and the
//! larger figure is the honest one to quote at a user: a bigger prompt is what
//! this code actually sends. PRD #802's own survey measured 4.49–4.68 s for
//! `claude` with `--output-format json`, which this does not ask for. Those
//! numbers predate the containment flags below, which have not been
//! re-measured; the flags remove work rather than adding it, so they are
//! unlikely to be slower, but nobody has checked.
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
//! # Containment: what this child is NOT allowed to do
//!
//! PRD #802's landed-work security audit found this backend spawning a
//! general-purpose coding agent with tools, hooks, MCP servers, skills, project
//! settings and session persistence all enabled, from the app's own working
//! directory, on a `PATH` that could name a relative entry. The downstream
//! action validator constrains only the JSON the CLI prints **after it exits**,
//! so anything the model did while producing that output had already happened.
//!
//! That matters because the prompt is built partly from input this app does not
//! control. The utterance is the obvious half. The other half is live state:
//! [`super::prompt::state`] puts agent **labels** in the prompt, and those come
//! from the daemon — which under [#741] can be a *remote* one. So the whole
//! prompt is treated as untrusted, and containment is what bounds a successful
//! injection rather than the validator alone.
//!
//! Every flag below was read off `claude --help` on 2026-09-19 with `claude
//! 2.1.277` and is quoted from it, and the exact argv is pinned by
//! `voice_agent_cli_contains_the_child`:
//!
//! | flag | what the CLI's own help says it does |
//! | --- | --- |
//! | `--tools ""` | "Specify the list of available tools from the built-in set. Use `""` to disable all tools" |
//! | `--safe-mode` | "Start with all customizations (CLAUDE.md, skills, plugins, hooks, MCP servers, custom commands and agents, output styles, workflows, custom themes, keybindings, and more) disabled" |
//! | `--restricted` | "removes the built-in tools that run commands or code … and ignores user, project and local settings files" |
//! | `--strict-mcp-config` | "Only use MCP servers from `--mcp-config`, ignoring all other MCP configurations" — and this passes no `--mcp-config`, so: none |
//! | `--setting-sources ""` | the settings sources to load, named explicitly as the empty set |
//! | `--permission-prompts none` | "nobody: anything that would prompt is denied automatically" — the fail-closed answer |
//! | `--no-session-persistence` | "sessions will not be saved to disk and cannot be resumed (only works with `--print`)" |
//!
//! Three of those overlap on purpose. `--tools ""` is the one that matters
//! most, and the other two tool-facing ones (`--restricted`, `--safe-mode`) are
//! there so a future CLI that reinterprets an empty `--tools` list does not
//! silently re-arm the child.
//!
//! **`--bare` looks like the flag for this job and is NOT used.** Its help says
//! "Anthropic auth is strictly `ANTHROPIC_API_KEY` or `apiKeyHelper` via
//! `--settings` (OAuth and keychain are never read)", which would break the one
//! property this backend exists for: that it works with the credential the user
//! already gave their CLI.
//!
//! Argument **order** is load-bearing and not cosmetic: `--tools` is variadic,
//! so the token after it has to start with `-` or the variadic would swallow
//! the prompt. Every containment flag therefore precedes the prompt, and the
//! prompt is last.
//!
//! ## The executable, the working directory and the environment
//!
//! - **Pinned.** The program is resolved to an **absolute** path
//!   ([`resolve_on_path`]) from a `PATH` with empty and relative components
//!   rejected, and that absolute path is what is executed. `Command::new
//!   ("claude")` would have accepted a `.` or a checkout-relative entry in an
//!   inherited or login-shell-derived `PATH` and run a repository-supplied
//!   `claude`.
//! - **Rehomed.** The child runs in an app-owned, empty working directory
//!   ([`app_owned_cwd`]) rather than inheriting the app's, so a project-local
//!   configuration or hook in whatever directory the app happens to be in
//!   cannot be picked up. The settings flags above already refuse those; this
//!   removes the directory as well as the permission.
//! - **Narrowed.** The environment is an allowlist ([`child_env`]) rather than
//!   an inheritance, so an unrelated secret in the app's environment is not in
//!   the child's. What survives is what the CLI needs to find its own
//!   credentials, reach the API through whatever proxy and CA the machine
//!   uses, and write a temporary file.
//!
//! # The five properties copied from `codex_hooks_manage::list_hooks_in`
//!
//! `src/codex_hooks_manage.rs:640` is this repo's one existing non-interactive
//! agent-CLI spawn, and PRD #802 names it as the shape to copy: a pinned child
//! environment, stderr discarded, a bounded wait behind a named constant, the
//! child killed before returning, and every failure mode an `Err`. All five are
//! here. The environment is no longer the divergence it was: that function pins
//! `CODEX_HOME` and this one used to inherit wholesale; it now allowlists, and
//! the allowlist is what keeps the CLI's own authentication reachable.
//!
//! [#741]: https://github.com/vfarcic/dot-agent-deck/issues/741

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;

use super::prompt::{MAX_SCAN_BYTES, cli_prompt, extract_answer};
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

/// How long the containment teardown gets after a timeout, before this gives up
/// on reaping and returns anyway.
///
/// The teardown is a `SIGKILL` to the whole process group, which cannot be
/// caught — so the only thing that can keep the reap waiting is a child wedged
/// in uninterruptible kernel I/O. That is not a reason to park the user's
/// utterance indefinitely on top of the twenty seconds already spent, so the
/// wait is bounded and the failure sentence goes out either way.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of the child's stdout is read before the attempt is abandoned.
///
/// Derived from [`MAX_SCAN_BYTES`] rather than chosen, because the two bound
/// the same thing from opposite ends and drifting apart would be silly: the
/// parser scans at most 64 KiB for the answer, so reading more than 64 KiB can
/// only grow an allocation that nothing will look at.
///
/// **This is the bound the audit asked for, and the point is WHERE it applies.**
/// `wait_with_output` — what this used to call — drains stdout into a growing
/// `Vec<u8>` and only then hands it to a parser that stops at 64 KiB, so a
/// confused or injected CLI could allocate for the whole twenty seconds. The
/// cap now applies while reading, before any UTF-8 conversion, and crossing it
/// is a fixed backend error rather than a bigger buffer.
const MAX_STDOUT_BYTES: usize = MAX_SCAN_BYTES;

/// The model the `claude` adapter pins.
///
/// The cheap fast tier, deliberately: this is short, closed-set, structured
/// output with no reasoning to do, which is the workload that tier is for, and
/// it is the same model PRD #802's survey measured and that
/// `docs/develop/config-gen-regeneration.md:49` already documents an
/// invocation of. Pinning it also stops a user's own default model — which may
/// be an expensive one — deciding what an utterance costs.
const CLAUDE_MODEL: &str = "claude-haiku-4-5";

/// Which CLI an [`AgentCliResolver`] drives.
///
/// **One variant today, and that is the truthful shape rather than an
/// oversight** — the same answer [`crate::settings::ActivationMode`] gives. PRD
/// #802 M5 shipped a second, `opencode`, and the landed-work security audit
/// withdrew it: `opencode run` has no equivalent of any flag in the containment
/// table above, no no-persistence option, and its sessions were confirmed
/// locally to be resumable and to hold the utterance. Shipping an uncontainable
/// subprocess executor in a feature that is visible by default is not a trade
/// this PRD is willing to make. [`crate::settings::IntentBackend`] carries the
/// decision where a reader of the settings schema will find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    /// `claude -p`, print mode. The default backend and the reference one for
    /// PRD #802 M9's phrase fixtures.
    Claude,
}

impl AgentCli {
    /// The program name, which is also what PATH resolution looks for.
    pub fn program(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
        }
    }

    /// What [`super::VoiceResult`] names as the backend that answered.
    pub fn backend_name(self) -> &'static str {
        self.program()
    }

    /// The argv after the program, with the prompt as ONE element and last.
    ///
    /// One element and not several: there is no shell here, so an utterance
    /// containing quotes, `$`, backticks or newlines is passed through by the
    /// kernel verbatim and cannot become syntax. That is a property of
    /// `Command`, not of any escaping this file does — which is why this file
    /// does none.
    ///
    /// Last, and after every flag, because `--tools` is variadic: a prompt
    /// placed between `--tools ""` and the next `-`-prefixed token would be
    /// read as a tool name rather than as the prompt. The module doc has the
    /// table of what each flag does.
    fn args(self, prompt: String) -> Vec<OsString> {
        match self {
            AgentCli::Claude => {
                let mut args: Vec<OsString> = vec![
                    OsString::from("-p"),
                    OsString::from("--model"),
                    OsString::from(CLAUDE_MODEL),
                ];
                args.extend(CLAUDE_CONTAINMENT.iter().map(OsString::from));
                args.push(OsString::from(prompt));
                args
            }
        }
    }
}

/// The flags that make the child safe to hand an untrusted prompt.
///
/// A flat slice rather than prose in [`AgentCli::args`] so the test that pins
/// the argv reads the same list the spawn does. The module doc quotes `claude
/// --help` for each one; the short version is *no tools, no customisations, no
/// MCP, no settings files, no permission grants, no session on disk*.
const CLAUDE_CONTAINMENT: &[&str] = &[
    "--tools",
    "",
    "--safe-mode",
    "--restricted",
    "--strict-mcp-config",
    "--setting-sources",
    "",
    "--permission-prompts",
    "none",
    "--no-session-persistence",
];

/// Environment variables the child keeps, by exact name.
///
/// The child is spawned with `env_clear`, so this list plus [`KEEP_PREFIX`] is
/// the whole of its environment (`PATH` excepted — see [`child_env`], which
/// installs the *sanitised* one rather than the inherited value).
///
/// The rule for being on it is **"the CLI cannot authenticate or reach the API
/// without it"**, not "it seems harmless". The groups, in order: where the CLI
/// finds its own credentials and configuration; who the user is, which a macOS
/// keychain read needs; a writable scratch directory; the Windows variables
/// without which sockets and TLS do not work at all; text handling, so a
/// non-ASCII utterance is not mangled; and the corporate TLS/proxy settings
/// that are the difference between reaching the API and not.
const KEEP_EXACT: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "USER",
    "LOGNAME",
    "USERNAME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "SystemRoot",
    "COMSPEC",
    "PATHEXT",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// Environment variables the child keeps, by prefix.
///
/// Prefixes rather than names because each of these families *is* an
/// authentication mechanism the CLI documents, and enumerating their members
/// here would go stale against the CLI rather than against this file:
/// `ANTHROPIC_*` for the direct API, `CLAUDE_*`/`CLAUDE_CODE_*` for the CLI's
/// own configuration (`CLAUDE_CONFIG_DIR` included), and the three cloud
/// families for the Bedrock and Vertex routes an enterprise install uses.
///
/// This is the deliberately *widest* part of the allowlist and it is worth
/// being honest about what it lets through: `AWS_SECRET_ACCESS_KEY` is on it.
/// It is on it because a Bedrock-backed CLI cannot authenticate without it —
/// that is exactly the "retain only the authentication mechanism required" rule
/// rather than an exception to it.
const KEEP_PREFIX: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_",
    "AWS_",
    "GOOGLE_",
    "GCLOUD_",
    "CLOUDSDK_",
];

/// Whether this resolver may go looking for the user's login-shell PATH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Spawn on the PATH this process inherited and nothing else. What the
    /// tests use, so no test can spend ten seconds in somebody's `~/.zshrc`.
    Inherit,
    /// Fall back to the user's login-shell PATH when the program is not found
    /// on the inherited one. The shipping behaviour — see
    /// [`AgentCliResolver::locate`] for why it is a fallback and not a startup
    /// step.
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
///   this uses the **capture** half, which mutates nothing, and resolves the
///   executable against the captured value.
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

/// Find `program` on `path`, as an **absolute** path, rejecting every `PATH`
/// component that is empty or relative.
///
/// This is the whole of the executable-pinning fix and it is a pure function so
/// it can be tested against a hostile `PATH` without touching the process
/// environment — which a test must not do, since `std::env::set_var` is a data
/// race against every other test in the binary.
///
/// **Why empty and relative components are dropped rather than resolved.** An
/// empty component means "the current directory" to every PATH implementation,
/// and a relative one means "relative to whatever the current directory
/// happens to be" — so either lets a directory the app merely *ran from*
/// supply the executable. A checkout containing a file called `claude` is not
/// an exotic scenario; it is a repository with a script in it. Resolving them
/// against the app-owned cwd instead would be worse, not better: it would make
/// the answer depend on a directory this module creates.
///
/// Returns the first component that yields a file this process can execute.
/// `None` means "not on this PATH", which the caller turns into
/// [`IntentError::NotConfigured`] — the one failure whose remedy is an
/// installation instruction.
fn resolve_on_path(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
        .find_map(|dir| executable_at(&dir.join(program)))
}

/// `candidate` if it names something this process can execute, else `None`.
///
/// On Unix that is the executable bit; on Windows it is the file's existence
/// under `candidate` itself and under each `PATHEXT` suffix, because a Windows
/// `claude` is `claude.cmd` or `claude.exe` and neither carries a mode bit.
fn executable_at(candidate: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(candidate).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| candidate.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        if candidate.is_file() {
            return Some(candidate.to_path_buf());
        }
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        extensions.split(';').find_map(|extension| {
            let extension = extension.trim();
            if extension.is_empty() {
                return None;
            }
            let mut name = candidate.as_os_str().to_os_string();
            name.push(extension);
            let with_extension = PathBuf::from(name);
            with_extension.is_file().then_some(with_extension)
        })
    }
}

/// The `PATH` the child is given: the inherited one with every empty and
/// relative component removed.
///
/// The child gets this rather than the raw inherited value for
/// [`resolve_on_path`]'s reason one level down — the CLI resolves programs of
/// its own, and handing it a `PATH` containing `.` would reinstate exactly the
/// substitution this module just closed for its own exec.
fn sanitised_path(path: &OsStr) -> OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
        .collect();
    std::env::join_paths(kept).unwrap_or_default()
}

/// An app-owned, empty directory to run the child in.
///
/// **Not the app's own working directory**, which is the defect this closes: a
/// desktop app launched from a terminal inherits that terminal's directory, so
/// the child would look for project configuration in whatever checkout the user
/// happened to be in. The containment flags already refuse to load it; this
/// removes the directory too, so the refusal is not the only thing standing
/// between an injected prompt and a project-local hook.
///
/// `state_dir()` is this app's own per-user state root, so the directory is
/// owned by the app in the sense that matters: nothing but this function writes
/// to it, and nothing at all writes *into* it — the child is given it as a cwd
/// and produces its answer on stdout.
///
/// Falls back to the system temp root when the directory cannot be created, and
/// **never** to the inherited cwd: a temp root is not a project, which is the
/// property being bought here.
fn app_owned_cwd() -> PathBuf {
    let dir = dot_agent_deck::platform::paths::state_dir().join("voice-cli-cwd");
    match std::fs::create_dir_all(&dir) {
        Ok(()) => dir,
        Err(_) => std::env::temp_dir(),
    }
}

/// The child's whole environment: [`KEEP_EXACT`] plus [`KEEP_PREFIX`], with
/// `PATH` replaced by `path`.
///
/// A pure function over `(inherited, path)` so the allowlist can be tested
/// without mutating the process environment. `inherited` is
/// [`std::env::vars_os`] in production.
fn child_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    path: &OsStr,
) -> Vec<(OsString, OsString)> {
    let mut kept: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| {
            let Some(name) = name.to_str() else {
                // A non-UTF-8 variable name cannot be matched against the
                // allowlist, so it is not on it.
                return false;
            };
            // PATH is installed below from the sanitised value, never inherited.
            if name.eq_ignore_ascii_case("PATH") {
                return false;
            }
            KEEP_EXACT.iter().any(|allowed| allowed == &name)
                || KEEP_PREFIX.iter().any(|prefix| name.starts_with(prefix))
        })
        .collect();
    kept.push((OsString::from("PATH"), path.to_os_string()));
    kept
}

/// Resolve intent by asking an agent CLI already installed on this machine.
pub struct AgentCliResolver {
    cli: AgentCli,
    /// What to spawn. The CLI's own name in production, resolved through
    /// [`resolve_on_path`]; an absolute path to a stub script under a
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
    ///
    /// The path given **must be absolute** — [`Self::locate`] refuses a
    /// relative one rather than resolving it against a directory this module
    /// chose, which would be the substitution hazard back by another door.
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

    fn not_installed(&self) -> IntentError {
        IntentError::NotConfigured(format!(
            "`{}` is not installed, or is not on this app's PATH",
            self.cli.program()
        ))
    }

    async fn run(&self, request: IntentRequest<'_>) -> Result<super::IntentAnswer, IntentError> {
        let (program, path) = self.locate().await.ok_or_else(|| self.not_installed())?;
        let prompt = cli_prompt(&request);

        let output = match self.spawn(&program, &path, &prompt).await {
            Ok(output) => output,
            Err(SpawnError::TimedOut) => {
                return Err(
                    self.failed(format!("did not answer within {}s", self.timeout.as_secs()))
                );
            }
            Err(SpawnError::TooMuchOutput) => {
                // Fixed wording, and deliberately not the byte count the child
                // actually reached: what the user can act on is "it flooded",
                // and the bound is this file's to state.
                return Err(self.failed(format!(
                    "printed more than {MAX_STDOUT_BYTES} bytes without an answer"
                )));
            }
            Err(SpawnError::Io(error)) => {
                return Err(self.failed(format!("could not be run ({error})")));
            }
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

    /// Where the CLI is, as an absolute path, and the sanitised `PATH` that
    /// found it.
    ///
    /// Both halves come back together because the child is given the same
    /// `PATH` the executable was resolved on — otherwise the CLI's own
    /// sub-lookups would run on a list this function had already rejected.
    ///
    /// The order is the same fallback [`login_shell_path`] documents: the
    /// inherited `PATH` first, and only a miss there pays for a login shell.
    async fn locate(&self) -> Option<(PathBuf, OsString)> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = sanitised_path(&inherited);

        // An explicit program — a test's stub, and the only way this field is
        // ever not the CLI's bare name — is used as given, provided it is
        // absolute and executable. A relative one is refused rather than
        // resolved, for `resolve_on_path`'s reason.
        if self.program.as_os_str() != OsStr::new(self.cli.program()) {
            let explicit = PathBuf::from(&self.program);
            return explicit
                .is_absolute()
                .then(|| executable_at(&explicit))
                .flatten()
                .map(|program| (program, path));
        }

        if let Some(found) = resolve_on_path(&self.program, &path) {
            return Some((found, path));
        }
        // The Finder case, and the only one that pays for a capture.
        let recovery = self.recovery_path().await?;
        let recovery = sanitised_path(OsStr::new(&recovery));
        let found = resolve_on_path(&self.program, &recovery)?;
        Some((found, recovery))
    }

    /// The login-shell PATH, when this resolver is allowed to go looking.
    async fn recovery_path(&self) -> Option<String> {
        match self.path_mode {
            PathMode::Inherit => None,
            PathMode::LoginShellFallback => login_shell_path().await,
        }
    }

    /// One bounded, contained, killed-on-every-path spawn.
    ///
    /// **The child is torn down before this returns on the timeout path**, and
    /// the unit torn down is the whole **process group**, not the direct child.
    /// `kill_on_drop(true)` is still set — it is what covers every *other* drop
    /// path, such as the surrounding resolve future being cancelled — but on
    /// its own it signals one pid, so a tool, hook or shell the CLI started
    /// would outlive the timeout with the app's descriptors still open. That
    /// was PRD #802's audit finding, and it is why this returns only after
    /// [`terminate_group`] has killed the group and the direct child has been
    /// reaped (or [`TEARDOWN_TIMEOUT`] has elapsed).
    ///
    /// Stdout is read through [`MAX_STDOUT_BYTES`] rather than with
    /// `wait_with_output`, so the bound applies while reading rather than after
    /// the whole stream has been allocated.
    async fn spawn(
        &self,
        program: &Path,
        path: &OsStr,
        prompt: &str,
    ) -> Result<std::process::Output, SpawnError> {
        let mut command = Command::new(program);
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
            .current_dir(app_owned_cwd())
            .env_clear()
            .envs(child_env(std::env::vars_os(), path))
            .kill_on_drop(true);
        // The containment unit. On Unix the child leads its own process group,
        // so one `killpg` reaches everything it started; on Windows the child
        // heads its own console process group and teardown walks the tree with
        // `taskkill /T`. See `terminate_group`.
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);

        let mut child = command.spawn().map_err(SpawnError::Io)?;
        let leader = child.id();
        let mut stdout = child.stdout.take().ok_or_else(|| {
            SpawnError::Io(std::io::Error::other(
                "the agent CLI produced no stdout pipe",
            ))
        })?;

        let collected = tokio::time::timeout(self.timeout, async {
            let bytes = read_capped(&mut stdout, MAX_STDOUT_BYTES).await?;
            let status = child.wait().await.map_err(SpawnError::Io)?;
            Ok::<_, SpawnError>((status, bytes))
        })
        .await;

        match collected {
            Ok(Ok((status, stdout))) => Ok(std::process::Output {
                status,
                stdout,
                stderr: Vec::new(),
            }),
            // A flood, or a read that failed: the child is still running and
            // may have started something, so it gets the same teardown a
            // timeout gets rather than being left to `kill_on_drop`, which
            // would signal the direct child alone.
            Ok(Err(error)) => {
                terminate_group(&mut child, leader).await;
                Err(error)
            }
            Err(_) => {
                terminate_group(&mut child, leader).await;
                Err(SpawnError::TimedOut)
            }
        }
    }
}

/// Why one [`AgentCliResolver::spawn`] did not produce output.
///
/// Three variants rather than an `io::Error` carrying an `ErrorKind`, because
/// two of these are this module's own decisions rather than the operating
/// system's: a timeout is a deadline this file chose, and a flood is a cap this
/// file set. Encoding them as `std::io::ErrorKind` values worked but meant the
/// caller classified this file's decisions by pattern-matching on a kind the
/// operating system can also produce.
#[derive(Debug)]
enum SpawnError {
    Io(std::io::Error),
    TimedOut,
    TooMuchOutput,
}

/// Read at most `cap` bytes, and fail rather than allocate past it.
///
/// Exactly `cap` bytes is a success: the cap is what the parser will look at,
/// so a reply that fills it exactly is still readable. `cap + 1` is
/// [`SpawnError::TooMuchOutput`], reported before anything converts the bytes
/// to text.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> Result<Vec<u8>, SpawnError> {
    let mut collected: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).await.map_err(SpawnError::Io)?;
        if read == 0 {
            return Ok(collected);
        }
        if collected.len() + read > cap {
            return Err(SpawnError::TooMuchOutput);
        }
        collected.extend_from_slice(&chunk[..read]);
    }
}

/// Windows: put the child at the head of its own console process group, so
/// teardown has a tree to walk rather than one pid.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Kill the whole containment unit and wait for the direct child to be reaped.
///
/// **Unix** is the strong half: the child was spawned with `process_group(0)`,
/// so its pid is also its pgid and one `killpg(SIGKILL)` reaches every
/// descendant that did not deliberately leave the group. `SIGKILL` rather than
/// a graceful escalation because this path is already twenty seconds past the
/// point where the answer was useful.
///
/// **Windows** is weaker and it is worth saying so plainly rather than implying
/// parity. The faithful analogue is a Job Object — which this repo already has,
/// in `dot_agent_deck::platform::proc::AgentProcessGroup` — but its terminate
/// half is private to that module and shaped around a `portable_pty::Child`,
/// and the alternative was writing fresh `windows-sys` FFI in this crate that
/// `build-windows` would compile and nothing in this repository can execute.
/// So Windows walks the tree with `taskkill /T /F`, which reaps the descendants
/// that are still parented under the child and misses one that has re-parented
/// itself. That is a real gap; it is a smaller one than the direct-child-only
/// kill it replaces.
///
/// Either way the reap is bounded by [`TEARDOWN_TIMEOUT`] so a wedged child
/// cannot hold the utterance open indefinitely.
async fn terminate_group(child: &mut tokio::process::Child, leader: Option<u32>) {
    if let Some(leader) = leader {
        #[cfg(unix)]
        {
            // SAFETY: `killpg(2)` takes a pgid and a signal and touches nothing
            // in this process. The pgid is the child's own pid — it was spawned
            // with `process_group(0)`, which makes it the group leader — and
            // `child` has not been reaped yet, so the pid cannot have been
            // recycled onto another process. A failure (`ESRCH`: the group is
            // already gone) is the ordinary case and is discarded.
            unsafe {
                libc::killpg(leader as libc::pid_t, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            // `taskkill` is resolved under `%SYSTEMROOT%` rather than through
            // PATH, for `resolve_on_path`'s reason: this is a teardown of a
            // process that may have been spawned by an injected prompt, and
            // resolving the killer through an attacker-influenced PATH would
            // be an odd way to end.
            let system_root =
                std::env::var("SYSTEMROOT").unwrap_or_else(|_| r"C:\Windows".to_string());
            let taskkill = PathBuf::from(system_root).join(r"System32\taskkill.exe");
            let _ = Command::new(taskkill)
                .args(["/T", "/F", "/PID", &leader.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await;
        }
    }
    // The direct child as well, in case it was never in the group we signalled,
    // and then the reap. `start_kill` on an already-dead child is not an error
    // worth reporting here.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(TEARDOWN_TIMEOUT, child.wait()).await;
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
            Self::named("stub-cli", body)
        }

        fn named(name: &str, body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(name);
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

    // -- containment: the argv ---------------------------------------------

    /// Every containment flag, in the order the spawn passes them, with the
    /// prompt LAST.
    ///
    /// Pinned as a whole list rather than asserted flag by flag because the
    /// order is a correctness property and not a style one: `--tools` is
    /// variadic, so a prompt that ended up between it and the next `-`-prefixed
    /// token would be read as a tool name. A test that only checked membership
    /// would pass on an argv that silently disabled nothing.
    #[test]
    fn voice_agent_cli_contains_the_child() {
        let argv = AgentCli::Claude.args("SAID".to_string());
        assert_eq!(
            argv,
            vec![
                OsString::from("-p"),
                OsString::from("--model"),
                OsString::from("claude-haiku-4-5"),
                OsString::from("--tools"),
                OsString::from(""),
                OsString::from("--safe-mode"),
                OsString::from("--restricted"),
                OsString::from("--strict-mcp-config"),
                OsString::from("--setting-sources"),
                OsString::from(""),
                OsString::from("--permission-prompts"),
                OsString::from("none"),
                OsString::from("--no-session-persistence"),
                OsString::from("SAID"),
            ]
        );
        // The prompt is last and every flag precedes it: the variadic `--tools`
        // must never be the option immediately before the prompt.
        assert_eq!(argv.last(), Some(&OsString::from("SAID")));
        let tools = argv
            .iter()
            .position(|arg| arg == "--tools")
            .expect("--tools is passed");
        assert!(
            argv[tools + 2].to_string_lossy().starts_with('-'),
            "the token after `--tools \"\"` must start with `-`, or the variadic eats the prompt"
        );
        // `--bare` would break the CLI's own authentication — see the module
        // doc. It must never be on this list.
        assert!(!argv.iter().any(|arg| arg == "--bare"));
    }

    /// Scenario: the child is asked to do something a tool would be needed for.
    /// The stub reports which tool-facing flags it was given; the assertion is
    /// that they were all present, so nothing the model decided while producing
    /// its answer could have run.
    ///
    /// The honest limit of this test, stated because the name could be read
    /// wider than it is: a stub cannot prove what the real `claude` does with
    /// `--tools ""`. What it proves is that the flags reach the child on the
    /// path a tool-requesting utterance takes, and that the utterance itself
    /// does not displace them — which is the half this repository owns.
    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_disables_tools_before_the_model_sees_the_utterance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let touched = dir.path().join("side-effect");
        // The stub stands in for a CLI that was talked into running a tool: if
        // the argv did NOT contain the containment flags it writes the file,
        // which is the pre-validation side effect the audit described.
        let stub = Stub::new(&format!(
            "tools=no; safe=no; persist=no\n\
             for arg in \"$@\"; do\n\
             case \"$arg\" in\n\
             --tools) tools=yes ;;\n\
             --safe-mode) safe=yes ;;\n\
             --no-session-persistence) persist=yes ;;\n\
             esac\n\
             done\n\
             if [ \"$tools$safe$persist\" != yesyesyes ]; then : > '{}'; fi\n\
             printf '{{\"action\":\"open_deck\"}}'",
            touched.to_string_lossy()
        ));
        let answer = ask(&stub.resolver(), "read every file in my home directory")
            .await
            .expect("answers");
        assert_eq!(answer.action, "open_deck");
        assert!(
            !touched.exists(),
            "the child ran without the containment flags"
        );
    }

    // -- containment: the executable ---------------------------------------

    /// Scenario: a PATH carrying the hostile entries — an empty component, `.`
    /// and a checkout-relative directory — alongside one absolute directory
    /// holding the genuine CLI. Resolution must return the absolute one, and
    /// must return nothing at all when only the hostile entries are present.
    ///
    /// **What this asserts and what it does not.** It asserts the resolution
    /// RULE — a component that is empty or relative is never consulted,
    /// whatever it holds, and the answer is always an absolute path. It does
    /// not stage a working substitution, because staging one means arranging
    /// the process's current directory to contain a file called `claude`, and a
    /// test may neither change that directory (it is process-wide, and every
    /// other test in this binary shares it) nor write into it. The rule is the
    /// property; a component that is never read cannot substitute anything.
    ///
    /// Pure-function rather than `set_var`-driven for the same reason: mutating
    /// the process environment races every other test in the binary.
    #[test]
    #[cfg(unix)]
    fn voice_agent_cli_refuses_a_relative_path_entry() {
        let real = Stub::named("claude", "printf '{\"action\":\"none\"}'");
        let real_dir = real.path.parent().expect("a parent").to_path_buf();
        // A relative directory that really does hold an executable `claude`:
        // the hostile entry an app launched from a checkout would see.
        let hostile = Stub::named("claude", "printf 'pwned'");
        let hostile_relative = std::path::PathBuf::from(
            hostile
                .path
                .parent()
                .expect("a parent")
                .strip_prefix("/")
                .expect("an absolute tempdir"),
        );
        assert!(hostile_relative.is_relative());

        let path = std::env::join_paths([
            std::path::PathBuf::from(""),
            std::path::PathBuf::from("."),
            hostile_relative.clone(),
            real_dir,
        ])
        .expect("a PATH");

        let found = resolve_on_path(OsStr::new("claude"), &path).expect("the real one is found");
        assert_eq!(
            found, real.path,
            "a relative PATH entry substituted the CLI"
        );
        assert!(found.is_absolute(), "an exec target must be absolute");

        // With only hostile entries there is no answer at all, rather than a
        // relative one that `Command::new` would resolve against whatever the
        // current directory happened to be at exec time.
        let hostile_only = std::env::join_paths([
            std::path::PathBuf::from(""),
            std::path::PathBuf::from("."),
            hostile_relative,
        ])
        .expect("a PATH");
        assert_eq!(resolve_on_path(OsStr::new("claude"), &hostile_only), None);
    }

    /// The same rule one level down: the PATH the CHILD is given has the empty
    /// and relative components removed too, so the CLI's own sub-lookups cannot
    /// be substituted either.
    #[test]
    #[cfg(unix)]
    fn voice_agent_cli_hands_the_child_a_sanitised_path() {
        let path = std::env::join_paths([
            std::path::PathBuf::from(""),
            std::path::PathBuf::from("."),
            std::path::PathBuf::from("relative/bin"),
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ])
        .expect("a PATH");
        let sanitised = sanitised_path(&path);
        let kept: Vec<_> = std::env::split_paths(&sanitised).collect();
        assert_eq!(
            kept,
            vec![
                std::path::PathBuf::from("/usr/bin"),
                std::path::PathBuf::from("/bin")
            ]
        );
    }

    /// Scenario: the child reports the directory it was started in. It must not
    /// be the directory this process is standing in — which is what a hostile
    /// project configuration would be sitting in — and the directory it IS
    /// started in must hold no project configuration.
    ///
    /// Asserted through the child's own `pwd` rather than through
    /// [`app_owned_cwd`]'s return value, which would only assert that a
    /// function returns what it returns.
    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_does_not_run_in_the_inherited_working_directory() {
        // The stub answers with its own working directory in a param, which is
        // the only thing that establishes where it ran.
        let stub = Stub::new(
            "printf '{\"action\":\"open_agent\",\"params\":{\"agent\":\"%s\"}}' \"$(pwd)\"",
        );
        let answer = ask(&stub.resolver(), "show me the tester")
            .await
            .expect("answers");
        let child_cwd = std::path::PathBuf::from(
            answer
                .params
                .get("agent")
                .expect("the stub reported its cwd"),
        );

        let inherited = std::env::current_dir().expect("a cwd");
        assert_ne!(
            child_cwd.canonicalize().unwrap_or(child_cwd.clone()),
            inherited.canonicalize().unwrap_or(inherited.clone()),
            "the child inherited this process's working directory"
        );
        // And the directory it did run in carries no project configuration for
        // an injected prompt to reach for.
        assert!(!child_cwd.join("CLAUDE.md").exists());
        assert!(!child_cwd.join(".claude").exists());
        assert!(!child_cwd.join(".mcp.json").exists());
    }

    /// The environment allowlist: the CLI's own authentication survives,
    /// everything else does not, and PATH is the sanitised one rather than the
    /// inherited value.
    #[test]
    fn voice_agent_cli_narrows_the_child_environment() {
        let inherited: Vec<(OsString, OsString)> = [
            ("HOME", "/home/someone"),
            ("ANTHROPIC_API_KEY", "sk-ant-kept"),
            ("CLAUDE_CONFIG_DIR", "/home/someone/.claude"),
            ("AWS_SECRET_ACCESS_KEY", "bedrock-kept"),
            ("HTTPS_PROXY", "http://proxy:3128"),
            ("PATH", "/inherited/and/replaced"),
            // Not an authentication mechanism of the selected CLI, so not kept.
            ("GITHUB_TOKEN", "ghp-dropped"),
            ("OPENAI_API_KEY", "sk-dropped"),
            ("DOT_AGENT_DECK_SOCKET", "/run/dropped.sock"),
            ("SSH_AUTH_SOCK", "/run/dropped-agent"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();

        let env = child_env(inherited, OsStr::new("/usr/bin:/bin"));
        let named = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };

        assert_eq!(named("HOME").as_deref(), Some("/home/someone"));
        assert_eq!(named("ANTHROPIC_API_KEY").as_deref(), Some("sk-ant-kept"));
        assert_eq!(
            named("CLAUDE_CONFIG_DIR").as_deref(),
            Some("/home/someone/.claude")
        );
        assert_eq!(
            named("AWS_SECRET_ACCESS_KEY").as_deref(),
            Some("bedrock-kept")
        );
        assert_eq!(named("HTTPS_PROXY").as_deref(), Some("http://proxy:3128"));

        assert_eq!(named("GITHUB_TOKEN"), None);
        assert_eq!(named("OPENAI_API_KEY"), None);
        assert_eq!(named("DOT_AGENT_DECK_SOCKET"), None);
        assert_eq!(named("SSH_AUTH_SOCK"), None);

        // PATH is present exactly once and is the value passed in, never the
        // inherited one.
        let paths: Vec<_> = env.iter().filter(|(key, _)| key == "PATH").collect();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].1, OsString::from("/usr/bin:/bin"));
    }

    // -- the bounded output ------------------------------------------------

    #[tokio::test]
    async fn voice_agent_cli_reads_exactly_the_cap_and_refuses_one_byte_more() {
        // Exact limit: readable. The cap is what the parser will scan, so a
        // reply that fills it exactly is still a reply.
        let exact = vec![b'x'; MAX_STDOUT_BYTES];
        let mut reader = exact.as_slice();
        let read = read_capped(&mut reader, MAX_STDOUT_BYTES)
            .await
            .expect("exactly the cap is readable");
        assert_eq!(read.len(), MAX_STDOUT_BYTES);

        // One byte more: refused, before anything converts it to text.
        let over = vec![b'x'; MAX_STDOUT_BYTES + 1];
        let mut reader = over.as_slice();
        let error = read_capped(&mut reader, MAX_STDOUT_BYTES)
            .await
            .expect_err("over the cap is refused");
        assert!(matches!(error, SpawnError::TooMuchOutput), "got {error:?}");
    }

    #[tokio::test]
    #[cfg_attr(not(unix), ignore = "the stub is a /bin/sh script")]
    async fn voice_agent_cli_refuses_a_flooding_child() {
        // A CLI that floods stdout is a fixed backend error rather than an
        // allocation that grows for the whole timeout.
        let stub = Stub::new(&format!(
            "i=0\nwhile [ $i -lt {} ]; do printf '%s' \
             'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; \
             i=$((i+1)); done",
            (MAX_STDOUT_BYTES / 64) + 16
        ));
        let error = ask(&stub.resolver(), "show me the tester")
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, IntentError::Backend(detail) if detail.contains("printed more than")),
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
    //
    // **The name used to overstate itself and now does not.** It checked the
    // stub shell's own pid and nothing else, so a `sleep 120` GRANDCHILD
    // survived the timeout with the app's descriptors open and the test still
    // passed — which is what PRD #802's audit found. The stub now forks a
    // grandchild that records its own pid, and both are asserted gone.
    #[cfg(unix)]
    #[tokio::test]
    async fn voice_agent_cli_times_out_and_leaves_no_child_or_grandchild_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("pid");
        let grandpidfile = dir.path().join("grandpid");
        let stub = Stub::new(&format!(
            "echo $$ > '{}'\n\
             sh -c 'echo $$ > \"{}\"; exec sleep 120' &\n\
             sleep 120",
            pidfile.to_string_lossy(),
            grandpidfile.to_string_lossy()
        ));
        let resolver = stub.resolver().with_timeout(Duration::from_millis(1000));

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

        let pid = read_pid(&pidfile);
        let grandpid = read_pid(&grandpidfile);
        assert_ne!(pid, grandpid, "the stub did not fork a real grandchild");
        assert!(
            wait_for_exit(pid, Duration::from_secs(5)),
            "pid {pid} survived the timeout"
        );
        // The finding this test exists for: `kill_on_drop` signals the direct
        // child only, so before the process-group teardown this grandchild ran
        // on for another two minutes.
        assert!(
            wait_for_exit(grandpid, Duration::from_secs(5)),
            "grandchild {grandpid} survived the timeout"
        );
    }

    /// The pid a stub wrote, waiting briefly for the write to land — the
    /// grandchild's `echo` races the parent's timeout by milliseconds.
    #[cfg(unix)]
    fn read_pid(path: &std::path::Path) -> i32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(pid) = raw.trim().parse::<i32>()
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the stub never recorded a pid at {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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
    }

    #[test]
    fn voice_agent_cli_pins_the_cheap_fast_tier() {
        let argv = AgentCli::Claude.args("SAID".to_string());
        assert_eq!(
            argv[..3].to_vec(),
            vec![
                OsString::from("-p"),
                OsString::from("--model"),
                OsString::from("claude-haiku-4-5"),
            ]
        );
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
        let resolver = AgentCliResolver::claude()
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
