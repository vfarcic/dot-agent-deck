//! PRD #1666 part 3 (Q35-13; plan §1.2): installation eligibility, decided
//! before a spawn from facts known before it, and what an installed spawn
//! gets.
//!
//! **The decision** ([`at_spawn`]) reads the platform, the pane's command
//! class ([`pane_class`]) and the rollout, never a release: no basename,
//! `PATH` lookup or version subprocess stands in for one, and the release a
//! generation's header later reports decides only whether the deck may arm
//! it ([`super::permission`]). The rollout is [`INSTALL_ROLLOUT`], empty
//! until activation, plus, in the `e2e` build only, the classes of the agents
//! the e2e assumption names ([`assumed_classes`]). That assumption is read
//! once, at daemon start ([`capture_assumption`]), where an unknown entry
//! refuses the whole list and the daemon goes on with no assumption, so no
//! generation is installed or armed by it; nothing reads the variable again.
//! A production build reads no variable.
//!
//! **An installed spawn** gets the instance's private helper
//! ([`super::instance`]) as `VISUAL` and `EDITOR`; the editor values it would
//! otherwise have had, saved for the helper ([`SAVED_VISUAL_ENV`],
//! [`SAVED_EDITOR_ENV`], each present only when the value was set, so unset
//! and exported-empty stay apart); its generation's name
//! ([`GENERATION_ENV`]); for a Claude-style class, `CLAUDE_ENV_FILE` naming a
//! file that restores those values for Claude's Bash tool, then sources the
//! user's own `CLAUDE_ENV_FILE` ([`env_file`]); and for a direct Codex,
//! `-c shell_environment_policy.set.VISUAL=…` and `…EDITOR=…` appended to its
//! command for the set values only, never an `exclude`, which would replace
//! the user's own list ([`codex_policy_args`]). Shells are never installed.
//! Any failure to set the helper up is [`NotInstalled::HelperSetup`], and the
//! pane spawns exactly as it would without installation.

use std::ffi::OsString;
use std::path::Path;
use std::sync::OnceLock;

use semver::Version;

use crate::doorbell_activation::EditorProbe;
use crate::event::AgentType;

#[cfg(target_os = "linux")]
use super::instance::{GenerationFiles, Instance};

/// Why an instance or one of its files couldn't be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupError {
    /// Every root's path holds a character outside `[A-Za-z0-9._/-]`, so an
    /// agent splitting `VISUAL` on spaces couldn't run the helper there.
    PathCharacters,
    /// A directory on the way down isn't this user's, can be written by its
    /// group or others, or isn't a directory (a symlink among them).
    UntrustedDirectory,
    /// The instance directory already exists: one is never reused.
    InstanceExists,
    /// The helper copy doesn't read as the executable it was copied from.
    CopyMismatch,
    /// A generation's name isn't one the instance files can carry.
    GenerationName,
    /// A file this generation needs exists already.
    GenerationExists,
    /// A value the spawn environment can't carry (not UTF-8).
    NotUtf8,
    /// An identity's install root doesn't read as the directory recorded.
    InstallRoot,
    /// A class that is never installed (a shell, an agent the probe doesn't
    /// cover) was asked to be.
    NotInstallable,
    /// Something else failed, with its errno when there is one.
    Io(Option<i32>),
}

impl From<std::io::Error> for SetupError {
    fn from(error: std::io::Error) -> Self {
        SetupError::Io(error.raw_os_error())
    }
}

/// An agent the composer probe covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAgent {
    Claude,
    Codex,
}

impl ProbeAgent {
    /// The probe's agent for `agent`, if it covers it.
    pub fn of(agent: &AgentType) -> Option<ProbeAgent> {
        match agent {
            AgentType::ClaudeCode => Some(ProbeAgent::Claude),
            AgentType::Codex => Some(ProbeAgent::Codex),
            _ => None,
        }
    }
}

/// A pane's command class, from its configuration and command alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneClass {
    /// The command's own argv is the agent's: its first word is the agent's
    /// executable, and it is a plain argv a shell runs as written.
    Direct(ProbeAgent),
    /// The agent, declared or recognised, runs below something else.
    Launcher(ProbeAgent),
    /// An undeclared command that isn't a shell and that no agent is
    /// recognised in (R5 §10.3).
    Wrapper,
    /// An agent the probe doesn't cover (OpenCode, Pi, Devin).
    Unprobed,
    /// A shell, or no command at all (the user's shell). Never installed.
    Shell,
}

/// The shells a pane's first word is never installed under, by basename.
pub const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "nu", "pwsh"];

/// The classes installation has rolled out to. Empty until activation:
/// Q35-28 adds Claude's direct and launcher classes and the wrapper class,
/// Q35-29 Codex's.
pub const INSTALL_ROLLOUT: &[PaneClass] = &[];

/// The class of a pane running `command`, its agent declared as `declared`.
pub fn pane_class(command: Option<&str>, declared: Option<&AgentType>) -> PaneClass {
    let Some(command) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        return PaneClass::Shell;
    };
    let tokens = crate::event::tokenize_command(command);
    let Some(first) = tokens.iter().find(|t| !crate::event::is_env_assignment(t)) else {
        return PaneClass::Shell;
    };
    let basename = Path::new(first)
        .file_name()
        .and_then(|b| b.to_str())
        .unwrap_or_default();
    if SHELLS.contains(&basename) {
        return PaneClass::Shell;
    }
    let agent = declared
        .filter(|a| **a != AgentType::None)
        .cloned()
        .or_else(|| AgentType::from_command(Some(command)));
    let Some(agent) = agent.filter(|a| *a != AgentType::None) else {
        return PaneClass::Wrapper;
    };
    let Some(probe) = ProbeAgent::of(&agent) else {
        return PaneClass::Unprobed;
    };
    let own = crate::agent_registry::spec(&agent)
        .detect_basenames
        .contains(&basename);
    if own && first == &tokens[0] && plain_argv(command) {
        PaneClass::Direct(probe)
    } else {
        PaneClass::Launcher(probe)
    }
}

/// Whether a shell runs `command` as one argv that ends where the command
/// does, so arguments appended to it reach its program: no control operator,
/// grouping, redirection, comment, substitution or newline.
fn plain_argv(command: &str) -> bool {
    !command.contains(['\n', ';', '&', '|', '(', ')', '<', '>', '#', '`', '\\'])
        && !command.contains("$(")
}

/// The classes the e2e assumption's agents roll out to: those Q35-28 adds
/// for Claude and Q35-29 for Codex.
pub fn assumed_classes(assumed: &[(AgentType, Version, EditorProbe<'_>)]) -> Vec<PaneClass> {
    let mut classes = Vec::new();
    for (agent, _, _) in assumed {
        let more: &[PaneClass] = match ProbeAgent::of(agent) {
            Some(ProbeAgent::Claude) => &[
                PaneClass::Direct(ProbeAgent::Claude),
                PaneClass::Launcher(ProbeAgent::Claude),
                PaneClass::Wrapper,
            ],
            Some(ProbeAgent::Codex) => &[
                PaneClass::Direct(ProbeAgent::Codex),
                PaneClass::Launcher(ProbeAgent::Codex),
            ],
            None => &[],
        };
        for class in more {
            if !classes.contains(class) {
                classes.push(*class);
            }
        }
    }
    classes
}

type Assumption = Vec<(AgentType, Version, EditorProbe<'static>)>;

static ASSUMPTION: OnceLock<Assumption> = OnceLock::new();

/// Read the e2e assumption once, at daemon start, through
/// [`crate::doorbell_activation::assumed_editor_probes`], which logs
/// `editor probe assumption refused` and assumes nothing for a list with an
/// unknown entry. Later calls return what the first read.
pub fn capture_assumption() -> &'static [(AgentType, Version, EditorProbe<'static>)] {
    ASSUMPTION.get_or_init(crate::doorbell_activation::assumed_editor_probes)
}

/// What [`capture_assumption`] read, or nothing when it hasn't run.
pub fn captured_assumption() -> &'static [(AgentType, Version, EditorProbe<'static>)] {
    ASSUMPTION.get().map_or(&[], Vec::as_slice)
}

/// Why a spawn isn't installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotInstalled {
    /// Shells never are.
    Shell,
    /// The class isn't in the rollout.
    Rollout,
    /// Not on this platform yet.
    Platform,
    /// The helper couldn't be set up.
    HelperSetup(SetupError),
}

/// Whether `class` is installed under `rollout`.
pub fn eligible(class: PaneClass, rollout: &[PaneClass]) -> Result<(), NotInstalled> {
    match class {
        PaneClass::Shell => Err(NotInstalled::Shell),
        _ if rollout.contains(&class) => Ok(()),
        _ => Err(NotInstalled::Rollout),
    }
}

/// The variable naming an installed spawn's generation.
pub const GENERATION_ENV: &str = "DOT_AGENT_DECK_EDITOR_GEN";
/// The spawn's own `VISUAL`, present only when it was set.
pub const SAVED_VISUAL_ENV: &str = "DOT_AGENT_DECK_EDITOR_USER_VISUAL";
/// The spawn's own `EDITOR`, present only when it was set.
pub const SAVED_EDITOR_ENV: &str = "DOT_AGENT_DECK_EDITOR_USER_EDITOR";
/// The helper's variables, which every spawn's child starts without, so one
/// left in the daemon's environment never names another daemon's instance.
pub const HELPER_ENV: [&str; 3] = [GENERATION_ENV, SAVED_VISUAL_ENV, SAVED_EDITOR_ENV];

/// The editor settings a spawn would have had without installation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Originals {
    pub visual: Option<String>,
    pub editor: Option<String>,
    pub claude_env_file: Option<String>,
}

impl Originals {
    /// What the child would see: the spawn's own value (the last, as the
    /// spawn applies them in order), else the one `inherited` gives.
    pub fn resolve(
        env: &[(String, String)],
        inherited: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Originals, SetupError> {
        let value = |name: &str| match env.iter().rev().find(|(k, _)| k == name) {
            Some((_, v)) => Ok(Some(v.clone())),
            None => inherited(name)
                .map(|v| v.into_string().map_err(|_| SetupError::NotUtf8))
                .transpose(),
        };
        Ok(Originals {
            visual: value("VISUAL")?,
            editor: value("EDITOR")?,
            claude_env_file: value("CLAUDE_ENV_FILE")?,
        })
    }
}

/// The editor settings the helper restores, read from its environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedEditor {
    pub visual: Option<OsString>,
    pub editor: Option<OsString>,
}

/// The saved editor settings in an environment `get` reads.
pub fn saved_editor(get: impl Fn(&str) -> Option<OsString>) -> SavedEditor {
    SavedEditor {
        visual: get(SAVED_VISUAL_ENV),
        editor: get(SAVED_EDITOR_ENV),
    }
}

/// `s` quoted for a POSIX shell: in single quotes, each `'` as `'\''`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `CLAUDE_ENV_FILE` of a Claude-style generation: `VISUAL` and `EDITOR`
/// back as they were (exported, exported empty, or unset), then the user's
/// own `CLAUDE_ENV_FILE`, if they had one, sourced after, so what it sets
/// still wins.
pub fn env_file(originals: &Originals) -> Vec<u8> {
    let mut s = String::from("# dot-agent-deck: the editor settings this agent started with.\n");
    for (name, value) in [("VISUAL", &originals.visual), ("EDITOR", &originals.editor)] {
        match value {
            Some(v) => s.push_str(&format!("export {name}={}\n", shell_quote(v))),
            None => s.push_str(&format!("unset {name}\n")),
        }
    }
    if let Some(theirs) = originals
        .claude_env_file
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        s.push_str(&format!(". {}\n", shell_quote(theirs)));
    }
    s.into_bytes()
}

/// A direct Codex's arguments giving its shell tool the set editor values.
pub fn codex_policy_args(originals: &Originals) -> Vec<String> {
    [("VISUAL", &originals.visual), ("EDITOR", &originals.editor)]
        .into_iter()
        .filter_map(|(name, value)| {
            let value = value.as_deref()?;
            Some([
                "-c".to_string(),
                format!("shell_environment_policy.set.{name}={}", toml_string(value)),
            ])
        })
        .flatten()
        .collect()
}

/// `command` with `args` appended, each quoted.
pub fn with_args(command: &str, args: &[String]) -> String {
    let mut out = command.to_string();
    for arg in args {
        out.push(' ');
        out.push_str(&shell_quote(arg));
    }
    out
}

/// What installation gives a spawn, and the generation's files while it
/// lives.
#[derive(Debug)]
pub struct Installation {
    class: PaneClass,
    env: Vec<(String, String)>,
    args: Vec<String>,
    #[cfg(target_os = "linux")]
    files: GenerationFiles,
}

impl Installation {
    /// The class installed.
    pub fn class(&self) -> PaneClass {
        self.class
    }

    /// The variables the spawn gets.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The arguments appended to the spawn's command.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The generation's files.
    #[cfg(target_os = "linux")]
    pub fn files(&mut self) -> &mut GenerationFiles {
        &mut self.files
    }

    /// `env` with the editor variables replaced by this installation's.
    pub fn apply_env(&self, env: &mut Vec<(String, String)>) {
        env.retain(|(k, _)| {
            !self.env.iter().any(|(set, _)| set == k) && !HELPER_ENV.contains(&k.as_str())
        });
        env.extend(self.env.iter().cloned());
    }

    /// `command` with this installation's arguments, when it has any.
    pub fn command(&self, command: Option<&str>) -> Option<String> {
        if self.args.is_empty() {
            return None;
        }
        command.map(|c| with_args(c, &self.args))
    }
}

/// Install generation `generation`, of class `class`, in `instance`: its
/// files, and the variables and arguments its spawn gets.
#[cfg(target_os = "linux")]
pub fn install(
    instance: &Instance,
    class: PaneClass,
    generation: &str,
    originals: &Originals,
) -> Result<Installation, SetupError> {
    let (kind, claude_style) = match class {
        PaneClass::Direct(agent) | PaneClass::Launcher(agent) => {
            (Some(agent), agent == ProbeAgent::Claude)
        }
        PaneClass::Wrapper => (None, true),
        PaneClass::Unprobed | PaneClass::Shell => return Err(SetupError::NotInstallable),
    };
    let env_contents = claude_style.then(|| env_file(originals));
    let files = instance.generation(generation, kind, env_contents.as_deref())?;
    let helper = instance
        .helper()
        .to_str()
        .ok_or(SetupError::NotUtf8)?
        .to_string();
    let mut env = vec![
        ("VISUAL".to_string(), helper.clone()),
        ("EDITOR".to_string(), helper),
        (GENERATION_ENV.to_string(), generation.to_string()),
    ];
    for (name, value) in [
        (SAVED_VISUAL_ENV, &originals.visual),
        (SAVED_EDITOR_ENV, &originals.editor),
    ] {
        if let Some(value) = value {
            env.push((name.to_string(), value.clone()));
        }
    }
    if let Some(path) = files.env_file() {
        let path = path.to_str().ok_or(SetupError::NotUtf8)?;
        env.push(("CLAUDE_ENV_FILE".to_string(), path.to_string()));
    }
    let args = match class {
        PaneClass::Direct(ProbeAgent::Codex) => codex_policy_args(originals),
        _ => Vec::new(),
    };
    Ok(Installation {
        class,
        env,
        args,
        files,
    })
}

/// The daemon's instance, made by the first installed spawn.
#[cfg(target_os = "linux")]
fn daemon_instance() -> Result<&'static Instance, SetupError> {
    use super::instance::{DaemonId, choose_root};
    static INSTANCE: OnceLock<Result<Instance, SetupError>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
            let root = choose_root(&crate::config::state_dir(), runtime.as_deref())?;
            let mut exe = std::fs::File::open("/proc/self/exe")?;
            let size = exe.metadata().map(|m| m.len()).unwrap_or_default();
            let instance = Instance::create(&root, DaemonId::this_process()?, &mut exe)?;
            let swept = instance.swept();
            tracing::info!(
                target: "composer_probe",
                dir = %instance.dir().display(),
                helper_bytes = size,
                removed = swept.removed.len(),
                kept = swept.kept,
                scan_error = ?swept.scan_error,
                "editor helper instance created"
            );
            Ok(instance)
        })
        .as_ref()
        .map_err(|e| *e)
}

/// The installation decision for a spawn of generation `generation` running
/// `command`, declared as `declared`, with `env` its own variables: whether
/// it is installed, and if so, everything it gets.
pub fn at_spawn(
    command: Option<&str>,
    declared: Option<&AgentType>,
    generation: &str,
    env: &[(String, String)],
) -> Result<Installation, NotInstalled> {
    let class = pane_class(command, declared);
    let mut rollout = INSTALL_ROLLOUT.to_vec();
    rollout.extend(assumed_classes(captured_assumption()));
    eligible(class, &rollout)?;
    #[cfg(target_os = "linux")]
    {
        let setup = || {
            let originals = Originals::resolve(env, |name| std::env::var_os(name))?;
            install(daemon_instance()?, class, generation, &originals)
        };
        setup().map_err(NotInstalled::HelperSetup)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (generation, env);
        Err(NotInstalled::Platform)
    }
}
