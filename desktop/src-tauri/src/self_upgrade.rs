//! Issue #1635: the desktop app notices a newer release and upgrades this
//! machine's copies in place — the app itself, and the CLI beside it.
//!
//! Nothing here decides anything about an upgrade. How a copy was installed,
//! what to offer for it, every word the user reads and the upgrade itself are
//! the root crate's [`dot_agent_deck::self_upgrade`], the same module
//! `dot-agent-deck upgrade` and the TUI use, so the two clients say the same
//! thing (CLAUDE.md rule 22). What this module owns is the desktop's half:
//!
//! - the host the core inspects: the real machine, searched with the user's
//!   login-shell `PATH` (captured once, the way the daemon captures it), since
//!   an app started from the Dock or a desktop launcher has a `PATH` that often
//!   lacks Homebrew and `~/.local/bin`;
//! - the plans the user was shown, kept so an Upgrade carries out exactly what
//!   the dialog said rather than whatever a second look would find;
//! - one upgrade at a time, run on a blocking thread because it waits on
//!   subprocesses (`brew`, `pkexec`, `hdiutil`) for as long as they take;
//! - the relaunch, offered only once the app bundle was actually replaced;
//! - the camelCase DTOs the webview renders.
//!
//! The app is the copy that can raise a graphical privilege prompt, so it
//! plans with `can_prompt_for_privilege`; when that prompt is dismissed or
//! fails, the result names the exact command instead.

use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dot_agent_deck::self_upgrade::{
    CopyKind, Host, Installation, OtherCopy, Outcome, PlanAction, PlanLine, PlanOptions,
    ProvenanceCheck, ReleaseSource, SystemHost, UPDATE_RECHECK_INTERVAL, UpgradeError, UpgradePlan,
    detect, discover, execute, plan, release_channel,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State, Webview};

use crate::dto::safe_message;

/// Which of this machine's two copies a plan or a result is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SelfCopy {
    /// This app (and the CLI bundled inside it).
    App,
    /// A separately installed `dot-agent-deck` CLI.
    Cli,
}

/// One line of a plan or a result. `text` is for display only and may be
/// shortened. `command`, on a line that is a command for the user to run, is
/// that command exactly as the core built it, never shortened or rewritten:
/// it is what Copy writes, and the dialog offers Copy only when its own
/// display sanitizer would leave it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LineDto {
    pub text: String,
    pub command: Option<String>,
}

/// Whether build provenance will be checked for a plan, and when not, why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProvenanceDto {
    pub checked: bool,
    pub reason: Option<String>,
}

/// [`UpgradePlan`] for the webview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlanDto {
    pub copy: SelfCopy,
    pub label: String,
    pub headline: String,
    pub current: String,
    pub latest: String,
    /// The [`PlanAction`] arm, kebab-case.
    pub action: &'static str,
    /// Whether Upgrade can carry it out.
    pub actionable: bool,
    /// The question the Upgrade button answers; `None` when there is nothing
    /// to confirm.
    pub confirm_question: Option<String>,
    /// Whether build provenance will be checked, decided before the user
    /// confirms (the lines say the same in words).
    pub provenance: ProvenanceDto,
    pub lines: Vec<LineDto>,
}

/// `desktop_self_upgrade_check`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckDto {
    /// The latest release, without a leading `v`.
    pub latest: String,
    /// Whether any copy is behind it. The notice shows only then.
    pub update_available: bool,
    /// The notice's text: the headline of the first copy that is behind — the
    /// app's, else the CLI's. The same words as the TUI's badge.
    pub notice: Option<String>,
    pub app: PlanDto,
    /// The CLI's own plan, when one is installed beside the app.
    pub cli: Option<PlanDto>,
    /// When to ask again ([`UPDATE_RECHECK_INTERVAL`]).
    pub recheck_after_secs: u64,
}

/// `desktop_self_upgrade_run`'s answer: what happened, in the core's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunDto {
    pub copy: SelfCopy,
    pub ok: bool,
    pub lines: Vec<LineDto>,
    /// Whether the app bundle was replaced, so a relaunch runs the new one.
    pub relaunch: bool,
}

/// The plans the user was last shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub app: UpgradePlan,
    pub cli: Option<UpgradePlan>,
}

impl Checked {
    fn plan(&self, copy: SelfCopy) -> Option<&UpgradePlan> {
        match copy {
            SelfCopy::App => Some(&self.app),
            SelfCopy::Cli => self.cli.as_ref(),
        }
    }
}

/// What this app holds between a check and an upgrade.
#[derive(Default, Clone)]
pub(crate) struct SelfUpgradeState {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The login shell's `PATH`; `Some(None)` once a capture failed.
    login_path: OnceLock<Option<OsString>>,
    checked: Mutex<Option<Checked>>,
    running: AtomicBool,
    relaunch_ready: AtomicBool,
}

/// The running upgrade's slot; released on drop, including on a panic or an
/// early return, so a failed run never leaves Upgrade refusing forever.
pub(crate) struct Running(Arc<Inner>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the webview reads when it asks to upgrade before checking.
pub(crate) const NOT_CHECKED: &str =
    "There is no checked upgrade for that copy. Check for a newer release first.";
/// What it reads when an upgrade is already running.
pub(crate) const ALREADY_RUNNING: &str = "An upgrade is already running.";
/// What it reads when it asks to relaunch before the app was replaced.
pub(crate) const NOTHING_TO_RELAUNCH: &str =
    "Agent Deck was not replaced, so there is nothing new to relaunch into.";

impl SelfUpgradeState {
    /// The login shell's `PATH`, captured on first use. Blocking.
    fn login_path(&self) -> Option<OsString> {
        self.inner
            .login_path
            .get_or_init(|| {
                dot_agent_deck::login_shell::capture_login_shell_path().map(OsString::from)
            })
            .clone()
    }

    fn store(&self, checked: Checked) {
        *lock(&self.inner.checked) = Some(checked);
    }

    /// The plan for `copy` the user was last shown, when it can be carried
    /// out.
    pub(crate) fn plan_for(&self, copy: SelfCopy) -> Result<UpgradePlan, String> {
        let checked = lock(&self.inner.checked);
        let plan = checked
            .as_ref()
            .and_then(|checked| checked.plan(copy))
            .ok_or_else(|| NOT_CHECKED.to_string())?;
        if !plan.is_actionable() {
            return Err(safe_message(plan.text()));
        }
        Ok(plan.clone())
    }

    /// Claim the one upgrade slot.
    pub(crate) fn begin(&self) -> Result<Running, String> {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return Err(ALREADY_RUNNING.to_string());
        }
        Ok(Running(self.inner.clone()))
    }

    fn mark_relaunch_ready(&self) {
        self.inner.relaunch_ready.store(true, Ordering::SeqCst);
    }

    pub(crate) fn relaunch_ready(&self) -> bool {
        self.inner.relaunch_ready.load(Ordering::SeqCst)
    }
}

/// The options this client plans with: staging where the terminal client
/// stages, a privilege prompt, which a desktop app can raise, and provenance
/// as `gh` on the login-shell `PATH` allows.
pub(crate) fn options(provenance: ProvenanceCheck) -> PlanOptions {
    PlanOptions {
        staging_root: PlanOptions::default_staging_root(),
        can_prompt_for_privilege: true,
        provenance,
    }
}

/// Plan the running app and, when one is installed, the CLI beside it, each
/// through its own install method, both against `latest`: the newest release
/// on the app's channel ([`release_channel`]). `path` is the `PATH` the CLI is
/// looked for on.
pub(crate) fn check_plans(
    host: &dyn Host,
    running: &Installation,
    latest: &str,
    options: &PlanOptions,
    path: Option<&OsStr>,
) -> Checked {
    let app = plan::plan(running, latest, options);
    let cli = match discover::other_copy(host, running, path) {
        OtherCopy::Found(other) => Some(plan::plan(&other, latest, options)),
        OtherCopy::NotFound | OtherCopy::NotOffered => None,
    };
    Checked { app, cli }
}

fn action_name(action: &PlanAction) -> &'static str {
    match action {
        PlanAction::UpToDate => "up-to-date",
        PlanAction::NotifyOnly => "notify-only",
        PlanAction::ShowCommand { .. } => "show-command",
        PlanAction::BrewUpgrade { .. } => "brew-upgrade",
        PlanAction::ReplaceBinary { .. } => "replace-binary",
        PlanAction::StagedInstall { .. } => "staged-install",
        PlanAction::InstallDeb { .. } => "install-deb",
        PlanAction::SwapApp { .. } => "swap-app",
        PlanAction::ManualDownload { .. } => "manual-download",
    }
}

/// The core's lines for the webview. A command keeps its exact text in
/// `command`; only the display copy goes through [`safe_message`], which
/// drops control characters and caps the length. The core builds no command
/// with a control or formatting character in it; one that somehow has one is
/// shown as prose and never offered for copying.
fn line_dtos(items: Vec<PlanLine>) -> Vec<LineDto> {
    items
        .into_iter()
        .map(|item| match item {
            PlanLine::Text(text) => LineDto {
                text: safe_message(text),
                command: None,
            },
            PlanLine::Command(command) => LineDto {
                text: safe_message(&command),
                command: (dot_agent_deck::untrusted_text::strip_control_and_bidi(&command, false)
                    == command)
                    .then_some(command),
            },
        })
        .collect()
}

fn provenance_dto(check: &ProvenanceCheck) -> ProvenanceDto {
    match check {
        ProvenanceCheck::Available { .. } => ProvenanceDto {
            checked: true,
            reason: None,
        },
        ProvenanceCheck::Unavailable { reason } => ProvenanceDto {
            checked: false,
            reason: Some(safe_message(reason)),
        },
    }
}

pub(crate) fn plan_dto(copy: SelfCopy, plan: &UpgradePlan) -> PlanDto {
    PlanDto {
        copy,
        label: plan.label().to_string(),
        headline: safe_message(plan.headline()),
        current: safe_message(&plan.installation.version),
        latest: safe_message(&plan.latest),
        action: action_name(&plan.action),
        actionable: plan.is_actionable(),
        confirm_question: plan.confirm_question().map(safe_message),
        provenance: provenance_dto(&plan.provenance),
        lines: line_dtos(plan.items()),
    }
}

pub(crate) fn check_dto(checked: &Checked) -> CheckDto {
    let behind = |plan: &UpgradePlan| plan.action != PlanAction::UpToDate;
    let notice = std::iter::once(&checked.app)
        .chain(checked.cli.as_ref())
        .find(|plan| behind(plan))
        .map(|plan| safe_message(plan.headline()));
    CheckDto {
        latest: safe_message(&checked.app.latest),
        update_available: notice.is_some(),
        notice,
        app: plan_dto(SelfCopy::App, &checked.app),
        cli: checked.cli.as_ref().map(|cli| plan_dto(SelfCopy::Cli, cli)),
        recheck_after_secs: UPDATE_RECHECK_INTERVAL.as_secs(),
    }
}

pub(crate) fn outcome_dto(copy: SelfCopy, outcome: &Outcome) -> RunDto {
    RunDto {
        copy,
        ok: true,
        lines: line_dtos(outcome.items()),
        relaunch: matches!(outcome, Outcome::AppReplaced { .. }),
    }
}

/// A failed upgrade for the webview: the error, then what the core says to do
/// instead. When the failure was the privilege prompt itself — dismissed,
/// refused, or `pkexec` failing — the verified file is still staged, and the
/// core hands over the exact command that installs it
/// ([`UpgradeError::fallback`]).
pub(crate) fn failure_dto(copy: SelfCopy, error: &UpgradeError) -> RunDto {
    let mut lines = vec![PlanLine::Text(error.to_string())];
    lines.extend(error.fallback());
    RunDto {
        copy,
        ok: false,
        lines: line_dtos(lines),
        relaunch: false,
    }
}

/// Check for a newer release, and plan this app and the CLI beside it.
///
/// An error is a check that could not be made (no network, GitHub refusing);
/// the webview shows no notice and asks again at the next interval.
#[tauri::command]
pub(crate) async fn desktop_self_upgrade_check(
    webview: Webview,
    state: State<'_, SelfUpgradeState>,
) -> Result<CheckDto, String> {
    crate::ensure_main_webview(&webview)?;
    let state = state.inner().clone();
    let detect_state = state.clone();
    let (path, running) = tauri::async_runtime::spawn_blocking(move || {
        let path = detect_state.login_path();
        let host = SystemHost { path: path.clone() };
        let running = detect::running(&host, CopyKind::Desktop).map_err(|e| e.to_string())?;
        Ok::<_, String>((path, running))
    })
    .await
    .map_err(|e| safe_message(e.to_string()))?
    .map_err(safe_message)?;
    // The CLI beside the app is planned against the same release, so both
    // copies follow the app's channel.
    let latest = ReleaseSource::from_build()
        .latest_version(release_channel(&running))
        .await
        .map_err(|e| safe_message(e.to_string()))?;
    let checked = tauri::async_runtime::spawn_blocking(move || {
        let host = SystemHost { path: path.clone() };
        let options = options(ProvenanceCheck::detect(&host));
        let checked = check_plans(&host, &running, &latest, &options, path.as_deref());
        state.store(checked.clone());
        checked
    })
    .await
    .map_err(|e| safe_message(e.to_string()))?;
    Ok(check_dto(&checked))
}

/// Carry out the plan for `copy` the user was shown, after they pressed
/// Upgrade. A failure is an `ok: false` result in the core's words, not an
/// error: it is what the dialog shows.
#[tauri::command]
pub(crate) async fn desktop_self_upgrade_run(
    webview: Webview,
    state: State<'_, SelfUpgradeState>,
    copy: SelfCopy,
) -> Result<RunDto, String> {
    crate::ensure_main_webview(&webview)?;
    let plan = state.plan_for(copy)?;
    let _running = state.begin()?;
    let state = state.inner().clone();
    let handle = tokio::runtime::Handle::current();
    let run_state = state.clone();
    let run_plan = plan.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let host = SystemHost {
            path: run_state.login_path(),
        };
        handle.block_on(execute::execute(
            &host,
            &run_plan,
            &ReleaseSource::from_build(),
            &PlanOptions::default_staging_root(),
        ))
    })
    .await
    .map_err(|e| safe_message(e.to_string()))?;
    Ok(match result {
        Ok(outcome) => {
            if matches!(outcome, Outcome::AppReplaced { .. }) {
                state.mark_relaunch_ready();
            }
            outcome_dto(copy, &outcome)
        }
        Err(error) => failure_dto(copy, &error),
    })
}

/// Restart the app onto the bundle an upgrade put in place. Refused unless
/// one did, so a stray call cannot restart the app for nothing.
#[tauri::command]
pub(crate) fn desktop_self_upgrade_relaunch(
    app: AppHandle,
    webview: Webview,
    state: State<'_, SelfUpgradeState>,
) -> Result<(), String> {
    crate::ensure_main_webview(&webview)?;
    if !state.relaunch_ready() {
        return Err(NOTHING_TO_RELAUNCH.to_string());
    }
    // `request_restart` rather than `restart`: it goes through
    // `RunEvent::ExitRequested`, so the app lets go of its terminals, tunnels
    // and microphone on the way out exactly as on a quit.
    app.request_restart();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::self_upgrade::{CommandOutput, Platform, Provenance};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    /// A machine made of a few files, the directories the user can write, and
    /// what each command line answers.
    #[derive(Default)]
    struct Fake {
        executables: HashSet<PathBuf>,
        links: HashMap<PathBuf, PathBuf>,
        writable: HashSet<PathBuf>,
        path: Vec<PathBuf>,
        home: Option<PathBuf>,
        answers: HashMap<String, CommandOutput>,
    }

    fn ok(stdout: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    impl Fake {
        fn linux() -> Self {
            Self {
                home: Some(PathBuf::from("/home/u")),
                ..Self::default()
            }
        }

        fn mac() -> Self {
            Self {
                home: Some(PathBuf::from("/Users/u")),
                ..Self::default()
            }
        }

        fn exe(mut self, path: &str) -> Self {
            self.executables.insert(PathBuf::from(path));
            self
        }

        fn deck(self, path: &str, version: &str) -> Self {
            self.exe(path).answer(
                &format!("{path} --version"),
                ok(&format!("dot-agent-deck {version}\n")),
            )
        }

        fn link(mut self, from: &str, to: &str) -> Self {
            self.links.insert(PathBuf::from(from), PathBuf::from(to));
            self
        }

        fn writable(mut self, dir: &str) -> Self {
            self.writable.insert(PathBuf::from(dir));
            self
        }

        fn on_path(mut self, dir: &str) -> Self {
            self.path.push(PathBuf::from(dir));
            self
        }

        fn answer(mut self, line: &str, output: CommandOutput) -> Self {
            self.answers.insert(line.to_string(), output);
            self
        }

        /// The `PATH` the app's login shell would report.
        fn login_path(&self) -> OsString {
            std::env::join_paths(&self.path).unwrap()
        }
    }

    impl Host for Fake {
        fn run(&self, program: &Path, args: &[&OsStr]) -> std::io::Result<CommandOutput> {
            let line = std::iter::once(program.as_os_str())
                .chain(args.iter().copied())
                .map(|w| w.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            match self.answers.get(&line) {
                Some(output) => Ok(output.clone()),
                None if self.executables.contains(program) => Ok(CommandOutput {
                    success: false,
                    code: Some(1),
                    ..CommandOutput::default()
                }),
                None => Err(std::io::ErrorKind::NotFound.into()),
            }
        }

        fn find_program(&self, name: &str) -> Option<PathBuf> {
            self.path
                .iter()
                .map(|dir| dir.join(name))
                .find(|candidate| self.is_executable(candidate))
        }

        fn is_executable(&self, path: &Path) -> bool {
            let path = self.links.get(path).map_or(path, PathBuf::as_path);
            self.executables.contains(path)
        }

        fn exists(&self, path: &Path) -> bool {
            self.executables.contains(path) || self.links.contains_key(path)
        }

        fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
            if let Some(target) = self.links.get(path) {
                return Some(target.clone());
            }
            self.exists(path).then(|| path.to_path_buf())
        }

        fn dir_writable(&self, dir: &Path) -> bool {
            self.writable.contains(dir)
        }

        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn is_wsl(&self) -> bool {
            false
        }
    }

    const CURRENT: &str = "0.46.0";
    const LATEST: &str = "0.47.0";
    const DEB_APP: &str = "/usr/bin/dot-agent-deck-desktop";
    const DMG_APP: &str = "/Applications/Agent Deck.app/Contents/MacOS/dot-agent-deck-desktop";

    fn test_options() -> PlanOptions {
        PlanOptions {
            staging_root: PathBuf::from("/home/u/.local/state/dot-agent-deck/upgrade"),
            can_prompt_for_privilege: true,
            provenance: ProvenanceCheck::Unavailable {
                reason: "the GitHub CLI (`gh`) is not installed".into(),
            },
        }
    }

    /// A Linux machine with the app installed from the `.deb`, its bundled
    /// CLI beside it, and `dpkg-query` answering for both.
    fn deb_machine() -> Fake {
        Fake::linux()
            .on_path("/usr/bin")
            .exe("/usr/bin/dpkg-query")
            .answer(
                &format!("/usr/bin/dpkg-query -S {DEB_APP}"),
                ok(&format!("agent-deck: {DEB_APP}\n")),
            )
            .deck("/usr/bin/dot-agent-deck", CURRENT)
            .answer(
                "/usr/bin/dpkg-query -S /usr/bin/dot-agent-deck",
                ok("agent-deck: /usr/bin/dot-agent-deck\n"),
            )
    }

    /// A Mac with the app in `/Applications`, signed when `team` is given.
    fn dmg_machine(writable: bool, team: Option<&str>) -> Fake {
        let mut host = Fake::mac();
        if writable {
            host = host.writable("/Applications");
        }
        if let Some(team) = team {
            host = host.exe("/usr/bin/codesign").answer(
                "/usr/bin/codesign -dv --verbose=2 /Applications/Agent Deck.app",
                CommandOutput {
                    success: true,
                    code: Some(0),
                    stdout: String::new(),
                    stderr: format!(
                        "Identifier=ai.devopstoolkit.agentdeck.desktop\nTeamIdentifier={team}\n"
                    ),
                },
            );
        }
        host
    }

    fn running(host: &Fake, exe: &str, platform: Platform, version: &str) -> Installation {
        detect::inspect(
            host,
            CopyKind::Desktop,
            Path::new(exe),
            version,
            Some(version),
            Some(platform),
        )
    }

    fn check(host: &Fake, exe: &str, platform: Platform) -> CheckDto {
        let path = host.login_path();
        let running = running(host, exe, platform, CURRENT);
        check_dto(&check_plans(
            host,
            &running,
            LATEST,
            &test_options(),
            Some(&path),
        ))
    }

    fn texts(lines: &[LineDto]) -> Vec<String> {
        lines.iter().map(|line| line.text.clone()).collect()
    }

    fn commands(lines: &[LineDto]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|line| line.command.clone())
            .collect()
    }

    #[test]
    fn self_upgrade_001_deb_app_with_pkexec_installs_behind_the_prompt() {
        let host = deb_machine().exe("/usr/bin/pkexec");
        let dto = check(&host, DEB_APP, Platform::LinuxAmd64);
        assert_eq!(dto.app.action, "install-deb");
        assert!(dto.app.actionable);
        assert_eq!(
            dto.app.confirm_question.as_deref(),
            Some("Upgrade Agent Deck (desktop app) to v0.47.0?")
        );
        let text = texts(&dto.app.lines).join("\n");
        assert!(text.contains("asks for your password"), "{text}");
        // The staged path is only known once the download is verified, so the
        // plan names no command yet.
        assert!(commands(&dto.app.lines).is_empty());
        // The `.deb`'s bundled CLI upgrades with the package: not a second copy.
        assert_eq!(dto.cli, None);
    }

    #[test]
    fn self_upgrade_002_deb_app_without_pkexec_shows_the_apt_command() {
        let dto = check(&deb_machine(), DEB_APP, Platform::LinuxAmd64);
        assert_eq!(dto.app.action, "install-deb");
        let text = texts(&dto.app.lines).join("\n");
        assert!(!text.contains("asks for your password"), "{text}");
        assert!(
            text.contains("is shown once the download is verified"),
            "{text}"
        );
        assert!(commands(&dto.app.lines).is_empty());
    }

    #[test]
    fn self_upgrade_003_dmg_in_applications_is_swapped_in_place() {
        let host = dmg_machine(true, Some("ABCDE12345"));
        let dto = check(&host, DMG_APP, Platform::MacosArm64);
        assert_eq!(dto.app.action, "swap-app");
        assert!(dto.app.actionable);
        let text = texts(&dto.app.lines).join("\n");
        assert!(text.contains("replaces the app"), "{text}");
        assert!(text.contains("restarts to run v0.47.0"), "{text}");
    }

    #[test]
    fn self_upgrade_004_dmg_in_an_unwritable_folder_says_what_to_do() {
        let host = dmg_machine(false, Some("ABCDE12345"));
        let dto = check(&host, DMG_APP, Platform::MacosArm64);
        assert_eq!(dto.app.action, "manual-download");
        assert!(!dto.app.actionable);
        assert_eq!(dto.app.confirm_question, None);
        let text = texts(&dto.app.lines).join("\n");
        assert!(
            text.contains("/Applications is not writable by you"),
            "{text}"
        );
        assert!(
            text.contains("v0.47.0/dot-agent-deck-desktop-alpha-macos-arm64.dmg"),
            "{text}"
        );
        // Still noticed: a copy that cannot be replaced from here is behind all
        // the same.
        assert!(dto.update_available);
    }

    #[test]
    fn self_upgrade_005_unsigned_app_says_to_download_the_new_dmg() {
        let host = dmg_machine(true, None);
        let dto = check(&host, DMG_APP, Platform::MacosArm64);
        assert_eq!(dto.app.action, "manual-download");
        let text = texts(&dto.app.lines).join("\n");
        assert!(text.contains("is not signed"), "{text}");
        assert!(text.contains("Download "), "{text}");
    }

    #[test]
    fn self_upgrade_006_up_to_date_shows_no_notice() {
        let host = deb_machine();
        let path = host.login_path();
        let running = running(&host, DEB_APP, Platform::LinuxAmd64, LATEST);
        let dto = check_dto(&check_plans(
            &host,
            &running,
            LATEST,
            &test_options(),
            Some(&path),
        ));
        assert!(!dto.update_available);
        assert_eq!(dto.notice, None);
        assert_eq!(dto.app.action, "up-to-date");
        assert_eq!(
            texts(&dto.app.lines),
            vec!["Agent Deck (desktop app) is up to date (v0.47.0)."]
        );
    }

    #[test]
    fn self_upgrade_007_notice_is_the_plans_headline() {
        let dto = check(&deb_machine(), DEB_APP, Platform::LinuxAmd64);
        assert!(dto.update_available);
        assert_eq!(
            dto.notice.as_deref(),
            Some("Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)")
        );
        assert_eq!(dto.notice.as_deref(), Some(dto.app.headline.as_str()));
        assert_eq!(dto.latest, "0.47.0");
    }

    #[test]
    fn self_upgrade_008_homebrew_cli_beside_the_app_is_brew_upgraded() {
        let keg = "/opt/homebrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck";
        let host = dmg_machine(true, Some("ABCDE12345"))
            .on_path("/opt/homebrew/bin")
            .exe("/opt/homebrew/bin/brew")
            .link("/opt/homebrew/bin/dot-agent-deck", keg)
            .deck(keg, CURRENT);
        let dto = check(&host, DMG_APP, Platform::MacosArm64);
        let cli = dto.cli.expect("the Homebrew CLI is found");
        assert_eq!(cli.copy, SelfCopy::Cli);
        assert_eq!(cli.action, "brew-upgrade");
        assert!(cli.actionable);
        assert_eq!(
            cli.confirm_question.as_deref(),
            Some("Upgrade dot-agent-deck to v0.47.0?")
        );
        assert!(
            texts(&cli.lines)
                .join("\n")
                .contains("brew upgrade dot-agent-deck")
        );
    }

    #[test]
    fn self_upgrade_009_writable_local_bin_cli_is_replaced_in_place() {
        let host = deb_machine()
            .on_path("/home/u/.local/bin")
            .deck("/home/u/.local/bin/dot-agent-deck", CURRENT)
            .writable("/home/u/.local/bin");
        let cli = check(&host, DEB_APP, Platform::LinuxAmd64)
            .cli
            .expect("the CLI in ~/.local/bin is found");
        assert_eq!(cli.action, "replace-binary");
        assert!(cli.actionable);
        assert!(commands(&cli.lines).is_empty());
    }

    #[test]
    fn self_upgrade_010_unwritable_cli_on_linux_raises_the_password_prompt() {
        let host = deb_machine()
            .exe("/usr/bin/pkexec")
            .on_path("/usr/local/bin")
            .deck("/usr/local/bin/dot-agent-deck", CURRENT);
        let cli = check(&host, DEB_APP, Platform::LinuxAmd64)
            .cli
            .expect("the CLI in /usr/local/bin is found");
        assert_eq!(cli.action, "staged-install");
        assert!(cli.actionable);
        let text = texts(&cli.lines).join("\n");
        assert!(text.contains("asks for your password"), "{text}");
        assert!(commands(&cli.lines).is_empty());
    }

    #[test]
    fn self_upgrade_011_unwritable_cli_on_macos_shows_the_command() {
        let host = dmg_machine(true, Some("ABCDE12345"))
            .on_path("/usr/local/bin")
            .deck("/usr/local/bin/dot-agent-deck", CURRENT);
        let cli = check(&host, DMG_APP, Platform::MacosArm64)
            .cli
            .expect("the CLI in /usr/local/bin is found");
        assert_eq!(cli.action, "staged-install");
        let text = texts(&cli.lines).join("\n");
        assert!(!text.contains("asks for your password"), "{text}");
        assert!(
            text.contains("is shown once the download is verified"),
            "{text}"
        );
    }

    #[test]
    fn self_upgrade_012_nix_cli_is_notify_only() {
        let store = "/nix/store/abc-dot-agent-deck-0.46.0/bin/dot-agent-deck";
        let host = deb_machine()
            .on_path("/home/u/.nix-profile/bin")
            .link("/home/u/.nix-profile/bin/dot-agent-deck", store)
            .deck(store, CURRENT);
        let cli = check(&host, DEB_APP, Platform::LinuxAmd64)
            .cli
            .expect("the Nix CLI is found");
        assert_eq!(cli.action, "notify-only");
        assert!(!cli.actionable);
        assert_eq!(cli.confirm_question, None);
        assert!(texts(&cli.lines).join("\n").contains("Installed with Nix"));
    }

    #[test]
    fn self_upgrade_013_source_built_cli_is_notify_only() {
        let built = "/home/u/code/dot-agent-deck/target/release/dot-agent-deck";
        let host = deb_machine()
            .on_path("/home/u/code/dot-agent-deck/target/release")
            .deck(built, CURRENT)
            .writable("/home/u/code/dot-agent-deck/target/release");
        let cli = check(&host, DEB_APP, Platform::LinuxAmd64)
            .cli
            .expect("the source-built CLI is found");
        assert_eq!(cli.action, "notify-only");
        assert!(texts(&cli.lines).join("\n").contains("Built from source"));
    }

    #[test]
    fn self_upgrade_014_no_cli_installed_offers_nothing_for_it() {
        let host = dmg_machine(true, Some("ABCDE12345"));
        let dto = check(&host, DMG_APP, Platform::MacosArm64);
        assert_eq!(dto.cli, None);
    }

    #[test]
    fn self_upgrade_015_cli_behind_an_up_to_date_app_is_still_noticed() {
        let host = deb_machine()
            .on_path("/home/u/.local/bin")
            .deck("/home/u/.local/bin/dot-agent-deck", CURRENT)
            .writable("/home/u/.local/bin");
        let path = host.login_path();
        let app = running(&host, DEB_APP, Platform::LinuxAmd64, LATEST);
        let dto = check_dto(&check_plans(
            &host,
            &app,
            LATEST,
            &test_options(),
            Some(&path),
        ));
        assert_eq!(dto.app.action, "up-to-date");
        assert!(dto.update_available);
        assert_eq!(
            dto.notice.as_deref(),
            Some("dot-agent-deck: update available: v0.47.0 (current: v0.46.0)")
        );
    }

    fn provenance() -> Provenance {
        Provenance::Skipped {
            reason: "the GitHub CLI (`gh`) is not installed".into(),
        }
    }

    #[test]
    fn self_upgrade_016_only_an_app_replacement_offers_relaunch() {
        let replaced = outcome_dto(
            SelfCopy::App,
            &Outcome::AppReplaced {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                version: LATEST.into(),
                provenance: provenance(),
            },
        );
        assert!(replaced.ok);
        assert!(replaced.relaunch);
        assert_eq!(
            replaced.lines[0].text,
            "Replaced /Applications/Agent Deck.app with v0.47.0. Quit and reopen Agent Deck to run it."
        );

        let others = [
            Outcome::Installed {
                version: LATEST.into(),
                provenance: provenance(),
            },
            Outcome::Replaced {
                path: PathBuf::from("/home/u/.local/bin/dot-agent-deck"),
                version: LATEST.into(),
                provenance: provenance(),
            },
            Outcome::BrewUpgraded {
                formula: "dot-agent-deck",
                reported: Some(LATEST.into()),
            },
            Outcome::Staged {
                path: PathBuf::from("/stage/x.deb"),
                command: Some("sudo apt install /stage/x.deb".into()),
                version: LATEST.into(),
                provenance: provenance(),
            },
        ];
        for outcome in &others {
            let dto = outcome_dto(SelfCopy::App, outcome);
            assert!(dto.ok);
            assert!(!dto.relaunch, "{outcome:?}");
        }
        let staged = outcome_dto(SelfCopy::App, &others[3]);
        assert_eq!(
            commands(&staged.lines),
            vec!["sudo apt install /stage/x.deb"]
        );
    }

    #[test]
    fn self_upgrade_017_a_dismissed_password_prompt_shows_the_command() {
        let install = "echo 'abc  /stage/v0.47.0-1f/x.deb' | sha256sum -c - && sudo apt install /stage/v0.47.0-1f/x.deb";
        let dismissed = UpgradeError::PrivilegeFailed {
            command: "/usr/bin/pkexec /usr/bin/apt-get install -y /stage/v0.47.0-1f/x.deb".into(),
            detail: "Error executing command as another user: Request dismissed".into(),
            install: Some(install.into()),
            version: LATEST.into(),
        };
        let dto = failure_dto(SelfCopy::App, &dismissed);
        assert!(!dto.ok);
        assert!(!dto.relaunch);
        assert!(dto.lines[0].text.contains("Request dismissed"));
        assert_eq!(dto.lines[1].text, plan::PROMPT_FAILED);
        assert_eq!(commands(&dto.lines), vec![install]);
    }

    #[test]
    fn self_upgrade_018_a_failure_before_the_prompt_offers_no_command() {
        let download = UpgradeError::Download {
            url: "https://example.invalid/x.deb".into(),
            detail: "timed out".into(),
        };
        let dto = failure_dto(SelfCopy::App, &download);
        assert!(!dto.ok);
        assert_eq!(dto.lines.len(), 1);
        assert!(commands(&dto.lines).is_empty());
    }

    #[test]
    fn self_upgrade_019_dto_is_camel_case_for_the_webview() {
        let host = deb_machine().exe("/usr/bin/pkexec");
        let json = serde_json::to_value(check(&host, DEB_APP, Platform::LinuxAmd64)).unwrap();
        assert_eq!(json["updateAvailable"], true);
        assert_eq!(json["recheckAfterSecs"], UPDATE_RECHECK_INTERVAL.as_secs());
        assert_eq!(json["app"]["copy"], "app");
        assert_eq!(json["app"]["action"], "install-deb");
        assert!(json["app"]["confirmQuestion"].is_string());
        assert!(json["app"]["lines"][0]["command"].is_null());
        assert_eq!(json["app"]["provenance"]["checked"], false);
        assert_eq!(
            json["app"]["provenance"]["reason"],
            "the GitHub CLI (`gh`) is not installed"
        );
        assert!(json["cli"].is_null());
        let copy: SelfCopy = serde_json::from_str("\"cli\"").unwrap();
        assert_eq!(copy, SelfCopy::Cli);
    }

    #[test]
    fn self_upgrade_020_upgrade_carries_out_only_a_checked_actionable_plan() {
        let state = SelfUpgradeState::default();
        assert_eq!(state.plan_for(SelfCopy::App).unwrap_err(), NOT_CHECKED);

        let notify_only = {
            let host = deb_machine()
                .on_path("/home/u/.nix-profile/bin")
                .link(
                    "/home/u/.nix-profile/bin/dot-agent-deck",
                    "/nix/store/abc/bin/dot-agent-deck",
                )
                .deck("/nix/store/abc/bin/dot-agent-deck", CURRENT)
                .exe("/usr/bin/pkexec");
            let path = host.login_path();
            let app = running(&host, DEB_APP, Platform::LinuxAmd64, CURRENT);
            check_plans(&host, &app, LATEST, &test_options(), Some(&path))
        };
        state.store(notify_only.clone());
        assert_eq!(state.plan_for(SelfCopy::App).unwrap(), notify_only.app);
        let refused = state.plan_for(SelfCopy::Cli).unwrap_err();
        assert!(refused.contains("Installed with Nix"), "{refused}");
    }

    #[test]
    fn self_upgrade_021_one_upgrade_at_a_time_and_no_relaunch_unless_replaced() {
        let state = SelfUpgradeState::default();
        let first = state.begin().unwrap();
        assert_eq!(state.begin().err().as_deref(), Some(ALREADY_RUNNING));
        drop(first);
        assert!(state.begin().is_ok());

        assert!(!state.relaunch_ready());
        state.mark_relaunch_ready();
        assert!(state.relaunch_ready());
    }

    #[test]
    fn self_upgrade_022_rechecks_on_the_shared_interval() {
        assert_eq!(UPDATE_RECHECK_INTERVAL.as_secs(), 6 * 60 * 60);
        let dto = check(&deb_machine(), DEB_APP, Platform::LinuxAmd64);
        assert_eq!(dto.recheck_after_secs, UPDATE_RECHECK_INTERVAL.as_secs());
    }

    #[test]
    fn self_upgrade_023_the_app_plans_with_a_privilege_prompt() {
        let gh = ProvenanceCheck::Available {
            gh: PathBuf::from("/usr/bin/gh"),
        };
        let options = options(gh.clone());
        assert!(options.can_prompt_for_privilege);
        assert_eq!(options.provenance, gh);
    }

    #[test]
    fn self_upgrade_024_a_long_command_reaches_the_webview_whole() {
        // Longer than `safe_message`'s cap: the display copy may be shortened,
        // the command Copy writes never is.
        let staged = format!(
            "/home/u/{}/dot-agent-deck-linux-amd64",
            "segment-".repeat(120)
        );
        let command = plan::install_binary_command(
            Some(Platform::LinuxAmd64),
            Path::new(&staged),
            Path::new("/usr/local/bin/dot-agent-deck"),
            &"a".repeat(64),
        )
        .expect("a long but clean path still gets a command");
        assert!(command.chars().count() > 2048);
        let dto = outcome_dto(
            SelfCopy::Cli,
            &Outcome::Staged {
                path: PathBuf::from(&staged),
                command: Some(command.clone()),
                version: LATEST.into(),
                provenance: provenance(),
            },
        );
        assert_eq!(commands(&dto.lines), vec![command.clone()]);
        assert!(command.ends_with(" /usr/local/bin/dot-agent-deck"));
    }

    #[test]
    fn self_upgrade_025_a_command_with_hidden_characters_is_never_copyable() {
        let dto = outcome_dto(
            SelfCopy::Cli,
            &Outcome::Staged {
                path: PathBuf::from("/stage/x"),
                command: Some("sudo install /stage/\u{202E}x /usr/local/bin/x".into()),
                version: LATEST.into(),
                provenance: provenance(),
            },
        );
        assert!(commands(&dto.lines).is_empty());
        // And the core shows no command for such a path in the first place:
        // the manual route instead.
        assert_eq!(
            plan::install_binary_command(
                Some(Platform::LinuxAmd64),
                Path::new("/stage/\u{202E}x"),
                Path::new("/usr/local/bin/dot-agent-deck"),
                &"a".repeat(64),
            ),
            None
        );
        let manual = outcome_dto(
            SelfCopy::Cli,
            &Outcome::Staged {
                path: PathBuf::from("/stage/x"),
                command: None,
                version: LATEST.into(),
                provenance: provenance(),
            },
        );
        assert!(commands(&manual.lines).is_empty());
        assert!(
            texts(&manual.lines)
                .join("\n")
                .contains("Upgrade manually from"),
            "{manual:?}"
        );
    }

    #[test]
    fn self_upgrade_026_the_plan_says_whether_provenance_will_be_checked() {
        let host = deb_machine();
        let path = host.login_path();
        let running = running(&host, DEB_APP, Platform::LinuxAmd64, CURRENT);
        let logged_out = PlanOptions {
            provenance: ProvenanceCheck::Unavailable {
                reason: "the GitHub CLI (`gh`) is not logged in, or its token is invalid (run `gh auth login`)".into(),
            },
            ..test_options()
        };
        let dto = check_dto(&check_plans(
            &host,
            &running,
            LATEST,
            &logged_out,
            Some(&path),
        ));
        assert!(!dto.app.provenance.checked);
        assert!(texts(&dto.app.lines).join("\n").contains(
            "Build provenance will NOT be checked: the GitHub CLI (`gh`) is not logged in"
        ),);
        let available = PlanOptions {
            provenance: ProvenanceCheck::Available {
                gh: PathBuf::from("/usr/bin/gh"),
            },
            ..test_options()
        };
        let dto = check_dto(&check_plans(
            &host,
            &running,
            LATEST,
            &available,
            Some(&path),
        ));
        assert_eq!(
            dto.app.provenance,
            ProvenanceDto {
                checked: true,
                reason: None
            }
        );
    }
}
