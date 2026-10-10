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
//! - the plans the user was shown, kept by check id so an Upgrade carries out
//!   exactly the plan the dialog said, never one a later check found, and an
//!   older check never replacing a newer one;
//! - the app's own copy once an upgrade installed it, so it is not offered
//!   again from the same old build and Relaunch stays reachable until the app
//!   restarts;
//! - one upgrade at a time, run on a blocking thread because it waits on
//!   subprocesses (`brew`, `pkexec`, `hdiutil`) for as long as they take,
//!   with the slot claimed before the plan is validated and held until the
//!   result is published;
//! - the copies whose privileged install did not finish and may still be
//!   running as root, which are not started again until the app restarts;
//! - the relaunch, offered only once the app bundle was actually replaced;
//! - the camelCase DTOs the webview renders.
//!
//! The app is the copy that can raise a graphical privilege prompt, so it
//! plans with `can_prompt_for_privilege`; when that prompt is dismissed or
//! fails, the result names the exact command instead.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use dot_agent_deck::self_upgrade::{
    CopyKind, Host, Installation, OtherCopy, Outcome, PlanAction, PlanLine, PlanOptions,
    ProvenanceCheck, ReleaseSource, Releases, SystemHost, UPDATE_RECHECK_INTERVAL, UpgradeError,
    UpgradePlan, detect, discover, execute, plan,
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
    /// Which check this is. Ids grow with every check; the dialog sends the
    /// one it shows with Upgrade, and exactly that check's plan is run.
    pub check_id: u64,
    /// The latest release, without a leading `v`.
    pub latest: String,
    /// Whether any copy is behind it and can still be offered. The notice
    /// shows then, and while a relaunch is pending.
    pub update_available: bool,
    /// The notice's text: the headline of the first copy that is behind — the
    /// app's, else the CLI's, the same words as the TUI's badge — or, once
    /// the app's own copy was installed and nothing else is behind, the
    /// core's relaunch line.
    pub notice: Option<String>,
    /// The app's own copy was installed this session and the app still runs
    /// the old build: its result, which the dialog shows in place of an
    /// offer, with Relaunch. `None` otherwise.
    pub installed: Option<RunDto>,
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

/// The plans one check found.
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
    /// The last check id handed out.
    last_check_id: AtomicU64,
    /// The newest checks, oldest first, at most [`KEPT_CHECKS`]: what a
    /// dialog showing one of them runs.
    checks: Mutex<VecDeque<(u64, Checked)>>,
    running: AtomicBool,
    /// The app's own copy, once an upgrade installed it.
    installed: Mutex<Option<Installed>>,
    /// The copies whose upgrade did not finish and may still be running as
    /// root ([`SelfUpgradeState::mark_unfinished`]).
    unfinished: Mutex<Vec<SelfCopy>>,
    /// Run by [`SelfUpgradeState::claim`] once the plan is validated, so a
    /// test can complete another upgrade at exactly that moment.
    #[cfg(test)]
    after_validate: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The app's own copy, installed this session and waiting for a relaunch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installed {
    /// The upgrade's result, as the dialog showed it.
    pub result: RunDto,
    /// The notice's text while nothing else is behind
    /// ([`Outcome::app_relaunch_notice`]).
    pub notice: String,
}

/// How many checks are kept for a dialog that is still showing one. A check
/// runs at start, every [`UPDATE_RECHECK_INTERVAL`] and when a dialog closes,
/// so a dialog would have to stay open for days to outlive this many.
pub(crate) const KEPT_CHECKS: usize = 8;

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
/// What it reads when the check its dialog shows is no longer kept.
pub(crate) const PLAN_GONE: &str = "The upgrade plan this dialog shows is no longer current, so nothing was changed. Close the dialog and open it again to see the current plan.";
/// What it reads, and what that copy's plan says, after an upgrade of the
/// copy did not finish and may still be running.
pub(crate) const INSTALL_MAY_BE_RUNNING: &str = "An earlier upgrade of this copy did not finish and may still be running, so it is not started again. Check which version is installed, and restart Agent Deck to upgrade it again.";
/// What it reads when it asks to upgrade the app again before relaunching.
pub(crate) const APP_ALREADY_INSTALLED: &str =
    "Agent Deck was already upgraded. Relaunch it to run the new version.";

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

    /// The id of a check about to start: higher than every one before it.
    pub(crate) fn next_check_id(&self) -> u64 {
        self.inner.last_check_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Keep check `id`'s plans, unless a newer check is already kept: a
    /// check that started earlier but finished later is dropped. Returns the
    /// newest check kept, which is what the webview is answered with.
    pub(crate) fn store(&self, id: u64, checked: Checked) -> (u64, Checked) {
        let mut checks = lock(&self.inner.checks);
        if checks.back().is_none_or(|(newest, _)| *newest < id) {
            checks.push_back((id, checked));
            while checks.len() > KEPT_CHECKS {
                checks.pop_front();
            }
        }
        checks.back().cloned().expect("a check was just kept")
    }

    /// The plan for `copy` that check `check_id` found — the check the dialog
    /// asking to carry it out shows — when it can be carried out.
    pub(crate) fn plan_for(&self, copy: SelfCopy, check_id: u64) -> Result<UpgradePlan, String> {
        if copy == SelfCopy::App && self.installed().is_some() {
            return Err(APP_ALREADY_INSTALLED.to_string());
        }
        if self.is_unfinished(copy) {
            return Err(INSTALL_MAY_BE_RUNNING.to_string());
        }
        let checks = lock(&self.inner.checks);
        if checks.is_empty() {
            return Err(NOT_CHECKED.to_string());
        }
        let checked = checks
            .iter()
            .find(|(id, _)| *id == check_id)
            .map(|(_, checked)| checked)
            .ok_or_else(|| PLAN_GONE.to_string())?;
        let plan = checked.plan(copy).ok_or_else(|| NOT_CHECKED.to_string())?;
        if !plan.is_actionable() {
            return Err(safe_message(plan.text()));
        }
        Ok(plan.clone())
    }

    /// Claim the one upgrade slot for `copy`, and the plan check `check_id`
    /// found for it, when it can be carried out ([`Self::plan_for`]).
    ///
    /// The slot is taken first and the plan validated under it, so no other
    /// upgrade can finish between the validation and the run: an app that an
    /// upgrade installed is marked installed before that upgrade lets go of
    /// the slot, and every later claim sees it. The caller holds the returned
    /// [`Running`] until it has published what its upgrade installed. A claim
    /// that fails validation lets go of the slot as it returns.
    pub(crate) fn claim(
        &self,
        copy: SelfCopy,
        check_id: u64,
    ) -> Result<(Running, UpgradePlan), String> {
        let running = self.begin()?;
        let plan = self.plan_for(copy, check_id)?;
        #[cfg(test)]
        if let Some(hook) = lock(&self.inner.after_validate).take() {
            hook();
        }
        Ok((running, plan))
    }

    /// Claim the one upgrade slot.
    pub(crate) fn begin(&self) -> Result<Running, String> {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return Err(ALREADY_RUNNING.to_string());
        }
        Ok(Running(self.inner.clone()))
    }

    /// Remember that the app's own copy was installed with `outcome`, shown
    /// as `result`. An outcome that installed nothing over the running app,
    /// or another copy's, is not remembered.
    pub(crate) fn mark_installed(&self, copy: SelfCopy, outcome: &Outcome, result: &RunDto) {
        if copy != SelfCopy::App {
            return;
        }
        if let Some(notice) = outcome.app_relaunch_notice() {
            *lock(&self.inner.installed) = Some(Installed {
                result: result.clone(),
                notice: safe_message(notice),
            });
        }
    }

    /// Remember that an upgrade of `copy` failed with `error`, when that is
    /// an install that may still be running as root
    /// ([`UpgradeError::InstallUnfinished`]). Until the app restarts, the
    /// copy is not started again — a second install would race the first —
    /// and its plan says so. This app cannot tell when that install ends: it
    /// runs as root, and the core only reaps it.
    pub(crate) fn mark_unfinished(&self, copy: SelfCopy, error: &UpgradeError) {
        if let UpgradeError::InstallUnfinished(unfinished) = error
            && unfinished.may_still_be_running.is_some()
        {
            let mut copies = lock(&self.inner.unfinished);
            if !copies.contains(&copy) {
                copies.push(copy);
            }
        }
    }

    fn is_unfinished(&self, copy: SelfCopy) -> bool {
        lock(&self.inner.unfinished).contains(&copy)
    }

    /// Check `check_id`'s plans for the webview, as this app's state leaves
    /// them: [`check_dto`], and a copy whose upgrade may still be running is
    /// not offered, its plan ending with why.
    pub(crate) fn answer(&self, check_id: u64, checked: &Checked) -> CheckDto {
        let mut dto = check_dto(check_id, checked, self.installed().as_ref());
        for plan in std::iter::once(&mut dto.app).chain(dto.cli.as_mut()) {
            if self.is_unfinished(plan.copy) {
                plan.actionable = false;
                plan.confirm_question = None;
                plan.lines.push(LineDto {
                    text: INSTALL_MAY_BE_RUNNING.to_string(),
                    command: None,
                });
            }
        }
        dto
    }

    pub(crate) fn installed(&self) -> Option<Installed> {
        lock(&self.inner.installed).clone()
    }

    pub(crate) fn relaunch_ready(&self) -> bool {
        self.installed().is_some()
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

/// The CLI installed beside the running app, when there is one the app
/// offers to upgrade. `path` is the `PATH` it is looked for on.
pub(crate) fn cli_beside(
    host: &dyn Host,
    running: &Installation,
    path: Option<&OsStr>,
) -> Option<Installation> {
    match discover::other_copy(host, running, path) {
        OtherCopy::Found(other) => Some(*other),
        OtherCopy::NotFound | OtherCopy::NotOffered => None,
    }
}

/// Plan the running app and `cli`, the CLI beside it when one is installed,
/// each through its own install method and on its own release channel, from
/// `releases` ([`ReleaseSource::releases_for`]).
pub(crate) fn plan_copies(
    running: &Installation,
    cli: Option<&Installation>,
    releases: &Releases,
    options: &PlanOptions,
) -> Checked {
    Checked {
        app: plan::plan(running, releases, options),
        cli: cli.map(|cli| plan::plan(cli, releases, options)),
    }
}

/// [`plan_copies`] for the CLI [`cli_beside`] finds.
#[cfg(test)]
pub(crate) fn check_plans(
    host: &dyn Host,
    running: &Installation,
    releases: &Releases,
    options: &PlanOptions,
    path: Option<&OsStr>,
) -> Checked {
    let cli = cli_beside(host, running, path);
    plan_copies(running, cli.as_ref(), releases, options)
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

/// Check `check_id`'s plans for the webview. Once the app's own copy is
/// `installed`, it is no longer offered — the running build is still the old
/// one, so a check would offer it again — and the notice names the next copy
/// that is behind, or, when none is, says to relaunch.
pub(crate) fn check_dto(
    check_id: u64,
    checked: &Checked,
    installed: Option<&Installed>,
) -> CheckDto {
    let behind = |plan: &UpgradePlan| plan.action != PlanAction::UpToDate;
    let offered = |copy: SelfCopy| !(copy == SelfCopy::App && installed.is_some());
    let headline = [
        (SelfCopy::App, Some(&checked.app)),
        (SelfCopy::Cli, checked.cli.as_ref()),
    ]
    .into_iter()
    .filter_map(|(copy, plan)| Some((copy, plan?)))
    .find(|(copy, plan)| offered(*copy) && behind(plan))
    .map(|(_, plan)| safe_message(plan.headline()));
    let mut app = plan_dto(SelfCopy::App, &checked.app);
    if installed.is_some() {
        app.actionable = false;
        app.confirm_question = None;
    }
    CheckDto {
        check_id,
        latest: safe_message(&checked.app.latest),
        update_available: headline.is_some(),
        notice: headline.or_else(|| installed.map(|installed| installed.notice.clone())),
        installed: installed.map(|installed| installed.result.clone()),
        app,
        cli: checked.cli.as_ref().map(|cli| plan_dto(SelfCopy::Cli, cli)),
        recheck_after_secs: UPDATE_RECHECK_INTERVAL.as_secs(),
    }
}

/// What an upgrade of `copy` did, for the webview. Relaunch is offered when
/// the app's own copy was installed: the `.dmg` swap, or the `.deb` installed
/// behind the password prompt, which replaces the running app's files.
pub(crate) fn outcome_dto(copy: SelfCopy, outcome: &Outcome) -> RunDto {
    RunDto {
        copy,
        ok: outcome.upgraded(),
        lines: line_dtos(outcome.items()),
        relaunch: copy == SelfCopy::App && outcome.app_relaunch_notice().is_some(),
    }
}

/// A failed upgrade for the webview: the error, then what the core says to do
/// instead ([`UpgradeError::fallback`]). When the failure was the privilege
/// prompt itself — dismissed, refused, or `pkexec` not running — the verified
/// file is still staged, and the core hands over the exact command that
/// installs it. When the install started and did not complete, the core says
/// what it found at the target and offers that command only when the target
/// is known not to be the new version.
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
    // Taken before anything is looked at, so a check that started later is
    // the newer one even when it finishes first.
    let check_id = state.next_check_id();
    let installed_state = state.clone();
    let detect_state = state.clone();
    let (path, running, cli) = tauri::async_runtime::spawn_blocking(move || {
        let path = detect_state.login_path();
        let host = SystemHost { path: path.clone() };
        let running = detect::running(&host, CopyKind::Desktop).map_err(|e| e.to_string())?;
        let cli = cli_beside(&host, &running, path.as_deref());
        Ok::<_, String>((path, running, cli))
    })
    .await
    .map_err(|e| safe_message(e.to_string()))?
    .map_err(safe_message)?;
    // Each copy is planned on its own channel: a stable CLI beside a
    // prerelease app is offered the newest stable release, and a CLI on the
    // beta formula the newest prerelease, the only kind its formula reaches.
    let releases = ReleaseSource::from_build()
        .releases_for(&running, cli.as_ref())
        .await
        .map_err(|e| safe_message(e.to_string()))?;
    let (check_id, checked) = tauri::async_runtime::spawn_blocking(move || {
        let host = SystemHost { path };
        let options = options(ProvenanceCheck::detect(&host));
        state.store(
            check_id,
            plan_copies(&running, cli.as_ref(), &releases, &options),
        )
    })
    .await
    .map_err(|e| safe_message(e.to_string()))?;
    Ok(installed_state.answer(check_id, &checked))
}

/// Carry out the plan for `copy` that check `check_id` found — the check the
/// dialog shows, so what runs is what the user read, whatever a later check
/// found — after they pressed Upgrade. A failure is an `ok: false` result in
/// the core's words, not an error: it is what the dialog shows.
#[tauri::command]
pub(crate) async fn desktop_self_upgrade_run(
    webview: Webview,
    state: State<'_, SelfUpgradeState>,
    copy: SelfCopy,
    check_id: u64,
) -> Result<RunDto, String> {
    crate::ensure_main_webview(&webview)?;
    // Held until the result is published below ([`SelfUpgradeState::claim`]).
    let (_running, plan) = state.claim(copy, check_id)?;
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
            let dto = outcome_dto(copy, &outcome);
            state.mark_installed(copy, &outcome, &dto);
            dto
        }
        Err(error) => {
            state.mark_unfinished(copy, &error);
            failure_dto(copy, &error)
        }
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
        fn run_within(
            &self,
            program: &Path,
            args: &[&OsStr],
            _timeout: std::time::Duration,
        ) -> std::io::Result<CommandOutput> {
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
        first_check(&check_plans(
            host,
            &running,
            &LATEST.into(),
            &test_options(),
            Some(&path),
        ))
    }

    /// The webview's answer for `checked`, as the first check, with nothing
    /// installed yet.
    fn first_check(checked: &Checked) -> CheckDto {
        check_dto(1, checked, None)
    }

    /// What a check finds on a Linux machine whose app came from the `.deb`
    /// (with `pkexec`) and whose CLI sits in a writable `~/.local/bin`, both
    /// at [`CURRENT`], against `latest`. Built from the install methods
    /// directly, so no machine is inspected.
    fn checked_against(latest: &str) -> Checked {
        let install = |copy, exe: &str, method| Installation {
            copy,
            executable: PathBuf::from(exe),
            version: CURRENT.into(),
            platform: Some(Platform::LinuxAmd64),
            method,
            tools: dot_agent_deck::self_upgrade::detect::Tools {
                pkexec: Some(PathBuf::from("/usr/bin/pkexec")),
                ..Default::default()
            },
        };
        let app = install(
            CopyKind::Desktop,
            DEB_APP,
            dot_agent_deck::self_upgrade::InstallMethod::DesktopDeb,
        );
        let cli_exe = "/home/u/.local/bin/dot-agent-deck";
        let cli = install(
            CopyKind::Cli,
            cli_exe,
            dot_agent_deck::self_upgrade::InstallMethod::DownloadedWritable {
                binary: PathBuf::from(cli_exe),
            },
        );
        plan_copies(&app, Some(&cli), &latest.into(), &test_options())
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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
        let dto = first_check(&check_plans(
            &host,
            &running,
            &LATEST.into(),
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn self_upgrade_015_cli_behind_an_up_to_date_app_is_still_noticed() {
        let host = deb_machine()
            .on_path("/home/u/.local/bin")
            .deck("/home/u/.local/bin/dot-agent-deck", CURRENT)
            .writable("/home/u/.local/bin");
        let path = host.login_path();
        let app = running(&host, DEB_APP, Platform::LinuxAmd64, LATEST);
        let dto = first_check(&check_plans(
            &host,
            &app,
            &LATEST.into(),
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
    fn self_upgrade_016_only_installing_the_app_itself_offers_relaunch() {
        let replaced = outcome_dto(
            SelfCopy::App,
            &Outcome::AppReplaced {
                app: PathBuf::from("/Applications/Agent Deck.app"),
                version: LATEST.into(),
                provenance: provenance(),
                mount_left: None,
            },
        );
        assert!(replaced.ok);
        assert!(replaced.relaunch);
        assert_eq!(
            replaced.lines[0].text,
            "Replaced /Applications/Agent Deck.app with v0.47.0. Quit and reopen Agent Deck to run it."
        );

        // The `.deb` installed behind the password prompt replaces the
        // running app's files too; the CLI's own install is not the app's.
        let deb = Outcome::Installed {
            version: LATEST.into(),
            provenance: provenance(),
        };
        assert!(outcome_dto(SelfCopy::App, &deb).relaunch);
        assert!(!outcome_dto(SelfCopy::Cli, &deb).relaunch);

        let others = [
            Outcome::BrewNotUpgraded {
                formula: "dot-agent-deck",
                reported: Some(CURRENT.into()),
                offered: LATEST.into(),
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
            assert_eq!(dto.ok, outcome.upgraded());
            assert!(!dto.relaunch, "{outcome:?}");
        }
        let staged = outcome_dto(SelfCopy::App, &others[3]);
        assert_eq!(
            commands(&staged.lines),
            vec!["sudo apt install /stage/x.deb"]
        );
    }

    #[test]
    fn self_upgrade_028_a_brew_upgrade_homebrew_could_not_deliver_is_a_failure() {
        let dto = outcome_dto(
            SelfCopy::Cli,
            &Outcome::BrewNotUpgraded {
                formula: "dot-agent-deck",
                reported: Some(CURRENT.into()),
                offered: LATEST.into(),
            },
        );
        assert!(!dto.ok);
        assert!(!dto.relaunch);
        assert_eq!(
            texts(&dto.lines),
            vec![
                "`brew upgrade dot-agent-deck` finished, but dot-agent-deck still reports v0.46.0, not v0.47.0: Homebrew does not offer v0.47.0 yet. Try again later."
            ]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn self_upgrade_020_upgrade_carries_out_only_a_checked_actionable_plan() {
        let state = SelfUpgradeState::default();
        assert_eq!(state.plan_for(SelfCopy::App, 1).unwrap_err(), NOT_CHECKED);

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
            check_plans(&host, &app, &LATEST.into(), &test_options(), Some(&path))
        };
        let id = state.next_check_id();
        state.store(id, notify_only.clone());
        assert_eq!(state.plan_for(SelfCopy::App, id).unwrap(), notify_only.app);
        let refused = state.plan_for(SelfCopy::Cli, id).unwrap_err();
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
        let replaced = Outcome::AppReplaced {
            app: PathBuf::from("/Applications/Agent Deck.app"),
            version: LATEST.into(),
            provenance: provenance(),
            mount_left: None,
        };
        // Another copy's outcome is not the app's.
        state.mark_installed(
            SelfCopy::Cli,
            &replaced,
            &outcome_dto(SelfCopy::Cli, &replaced),
        );
        assert!(!state.relaunch_ready());
        state.mark_installed(
            SelfCopy::App,
            &replaced,
            &outcome_dto(SelfCopy::App, &replaced),
        );
        assert!(state.relaunch_ready());
    }

    #[test]
    fn self_upgrade_029_an_older_check_never_replaces_a_newer_one() {
        let state = SelfUpgradeState::default();
        let older = state.next_check_id();
        let newer = state.next_check_id();
        assert!(newer > older);
        // The newer check finishes first; the older one, finishing later, is
        // dropped, and its caller is answered with the newer one.
        let (kept, checked) = state.store(newer, checked_against("0.48.0"));
        assert_eq!((kept, checked.app.latest.as_str()), (newer, "0.48.0"));
        let (kept, checked) = state.store(older, checked_against(LATEST));
        assert_eq!((kept, checked.app.latest.as_str()), (newer, "0.48.0"));
        let answer = check_dto(kept, &checked, None);
        assert_eq!(answer.check_id, newer);
        assert_eq!(answer.latest, "0.48.0");
        assert!(state.plan_for(SelfCopy::App, older).is_err());
    }

    #[test]
    fn self_upgrade_030_upgrade_runs_the_plan_the_dialog_shows() {
        let state = SelfUpgradeState::default();
        let shown = state.next_check_id();
        state.store(shown, checked_against(LATEST));
        // A background check finds a newer release while the dialog is open.
        let later = state.next_check_id();
        state.store(later, checked_against("0.48.0"));
        assert_eq!(state.plan_for(SelfCopy::App, shown).unwrap().latest, LATEST);
        assert_eq!(state.plan_for(SelfCopy::Cli, shown).unwrap().latest, LATEST);
        assert_eq!(
            state.plan_for(SelfCopy::App, later).unwrap().latest,
            "0.48.0"
        );

        // A dialog left open across more checks than are kept is refused,
        // never handed another plan.
        for _ in 0..KEPT_CHECKS {
            let id = state.next_check_id();
            state.store(id, checked_against("0.49.0"));
        }
        assert_eq!(state.plan_for(SelfCopy::App, shown).unwrap_err(), PLAN_GONE);
        assert_eq!(state.plan_for(SelfCopy::App, 999).unwrap_err(), PLAN_GONE);
        let json = serde_json::to_value(check_dto(shown, &checked_against(LATEST), None)).unwrap();
        assert_eq!(json["checkId"], shown);
    }

    #[test]
    fn self_upgrade_031_an_installed_app_is_not_offered_again_and_the_notice_says_relaunch() {
        let state = SelfUpgradeState::default();
        let id = state.next_check_id();
        let checked = state.store(id, checked_against(LATEST)).1;
        // The `.deb` installed behind the password prompt replaces the running
        // app's files, as the `.dmg` swap does.
        let installed = Outcome::Installed {
            version: LATEST.into(),
            provenance: provenance(),
        };
        let result = outcome_dto(SelfCopy::App, &installed);
        assert!(result.ok && result.relaunch);
        state.mark_installed(SelfCopy::App, &installed, &result);

        // The next check still sees the old build running, so it plans the
        // app again; the app is not offered, the CLI behind it is.
        let answer = check_dto(id, &checked, state.installed().as_ref());
        assert!(!answer.app.actionable);
        assert_eq!(answer.app.confirm_question, None);
        assert_eq!(answer.installed.as_ref(), Some(&result));
        assert!(answer.update_available);
        assert_eq!(
            answer.notice.as_deref(),
            Some("dot-agent-deck: update available: v0.47.0 (current: v0.46.0)")
        );
        assert_eq!(
            state.plan_for(SelfCopy::App, id).unwrap_err(),
            APP_ALREADY_INSTALLED
        );
        assert!(state.plan_for(SelfCopy::Cli, id).is_ok());

        // With nothing else behind, the notice is the relaunch prompt.
        let mut app_only = checked.clone();
        app_only.cli = None;
        let answer = check_dto(id, &app_only, state.installed().as_ref());
        assert!(!answer.update_available);
        assert_eq!(
            answer.notice.as_deref(),
            Some("Agent Deck v0.47.0 is installed. Relaunch to run it.")
        );
        assert!(state.relaunch_ready());
        let json = serde_json::to_value(&answer).unwrap();
        assert_eq!(json["installed"]["relaunch"], true);

        // The CLI's own install is not the app's.
        let cli_installed = outcome_dto(SelfCopy::Cli, &installed);
        assert!(cli_installed.ok && !cli_installed.relaunch);
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
        let dto = first_check(&check_plans(
            &host,
            &running,
            &LATEST.into(),
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
        let dto = first_check(&check_plans(
            &host,
            &running,
            &LATEST.into(),
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

    // Unix install layout: native Windows is unsupported (#164).
    #[cfg(unix)]
    #[test]
    fn self_upgrade_027_a_beta_formula_cli_is_offered_only_a_prerelease_and_told_to_switch() {
        const SWITCH: &str =
            "brew uninstall dot-agent-deck-beta && brew install vfarcic/tap/dot-agent-deck";
        let keg = "/opt/homebrew/Cellar/dot-agent-deck-beta/0.47.0-beta.1/bin/dot-agent-deck";
        let host = dmg_machine(true, Some("ABCDE12345"))
            .on_path("/opt/homebrew/bin")
            .exe("/opt/homebrew/bin/brew")
            .link("/opt/homebrew/bin/dot-agent-deck", keg)
            .deck(keg, "0.47.0-beta.1");
        let path = host.login_path();
        let app = running(&host, DMG_APP, Platform::MacosArm64, CURRENT);
        let check = |releases: Releases| {
            first_check(&check_plans(
                &host,
                &app,
                &releases,
                &test_options(),
                Some(&path),
            ))
        };

        // Only a newer stable: the app is offered it, the CLI is told its
        // formula cannot reach it and how to switch, with nothing to confirm.
        let dto = check(Releases {
            latest: "0.47.0".into(),
            stable: Some("0.47.0".into()),
            prerelease: Some("0.47.0-beta.1".into()),
        });
        assert_eq!(dto.app.action, "swap-app");
        let cli = dto.cli.expect("the Homebrew CLI is found");
        assert_eq!(cli.action, "notify-only");
        assert!(!cli.actionable);
        assert_eq!(cli.confirm_question, None);
        assert_eq!(cli.latest, "0.47.0");
        let text = texts(&cli.lines).join("\n");
        assert!(text.contains("does not carry stable releases"), "{text}");
        assert!(!text.contains("brew upgrade dot-agent-deck-beta"), "{text}");
        assert_eq!(commands(&cli.lines), vec![SWITCH.to_string()]);

        // A newer prerelease and a higher stable: the prerelease is brew
        // upgraded, and the stable switch is still mentioned.
        let dto = check(Releases {
            latest: "0.47.0".into(),
            stable: Some("0.47.0".into()),
            prerelease: Some("0.47.0-beta.3".into()),
        });
        let cli = dto.cli.expect("the Homebrew CLI is found");
        assert_eq!(cli.action, "brew-upgrade");
        assert!(cli.actionable);
        assert_eq!(
            cli.confirm_question.as_deref(),
            Some("Upgrade dot-agent-deck to v0.47.0-beta.3?")
        );
        let text = texts(&cli.lines).join("\n");
        assert!(text.contains("brew upgrade dot-agent-deck-beta"), "{text}");
        assert!(text.contains("Stable release v0.47.0"), "{text}");
        assert_eq!(commands(&cli.lines), vec![SWITCH.to_string()]);
    }

    /// Scenario: two Upgrade requests for the app arrive together, the second
    /// from a dialog still showing an older check that offered an older
    /// release. The first install completes at the moment the second has
    /// validated its plan. The second never runs: not while the first holds
    /// the slot, and not after it finished, when the app is installed and
    /// waits for Relaunch. Exactly one upgrade of the app runs.
    #[test]
    fn self_upgrade_032_a_second_upgrade_never_runs_over_the_app_just_installed() {
        let state = SelfUpgradeState::default();
        let older = state.next_check_id();
        state.store(older, checked_against("0.46.1"));
        let newer = state.next_check_id();
        state.store(newer, checked_against(LATEST));

        let (first, plan) = state.claim(SelfCopy::App, newer).unwrap();
        assert_eq!(plan.latest, LATEST);

        // The first install completes, marks the app installed and lets go
        // of the slot, right after the second request validated its plan.
        let completing = state.clone();
        let complete = move || {
            let installed = Outcome::Installed {
                version: LATEST.into(),
                provenance: provenance(),
            };
            completing.mark_installed(
                SelfCopy::App,
                &installed,
                &outcome_dto(SelfCopy::App, &installed),
            );
            drop(first);
        };
        *lock(&state.inner.after_validate) = Some(Box::new(complete));
        for check_id in [older, newer] {
            if let Ok((_running, plan)) = state.claim(SelfCopy::App, check_id) {
                panic!("v{} ran over the app just installed", plan.latest);
            }
        }
        // If no request reached the hook, the first install completes now.
        if let Some(complete) = lock(&state.inner.after_validate).take() {
            complete();
        }
        for check_id in [older, newer] {
            assert_eq!(
                state.claim(SelfCopy::App, check_id).err().as_deref(),
                Some(APP_ALREADY_INSTALLED)
            );
        }
        // A refused claim never keeps the slot.
        assert!(state.begin().is_ok());
    }

    fn unfinished(may_still_be_running: bool) -> UpgradeError {
        UpgradeError::InstallUnfinished(Box::new(
            dot_agent_deck::self_upgrade::UnfinishedInstall {
                command: "/usr/bin/pkexec /usr/bin/apt-get install -y /stage/x.deb".into(),
                detail: "it did not finish within 15 minutes and could not be stopped, so it may still be running".into(),
                may_still_be_running: may_still_be_running
                    .then_some(dot_agent_deck::self_upgrade::Interruption::TimedOut),
                target: dot_agent_deck::self_upgrade::InstallTarget::Package,
                found: dot_agent_deck::self_upgrade::Found::NotChecked,
                install: None,
                version: LATEST.into(),
            },
        ))
    }

    /// Scenario: the app's privileged install outlives its bound and cannot
    /// be stopped, so it may still be running as root. The result says to
    /// wait and check, offering no install command, and until the app
    /// restarts the same copy is not offered or started again; the CLI
    /// beside it still is. An install that was stopped blocks nothing.
    #[test]
    fn self_upgrade_033_an_install_that_may_still_be_running_is_not_started_again() {
        let state = SelfUpgradeState::default();
        let id = state.next_check_id();
        let checked = state.store(id, checked_against(LATEST)).1;
        let error = unfinished(true);
        let dto = failure_dto(SelfCopy::App, &error);
        assert!(!dto.ok && !dto.relaunch);
        assert!(
            commands(&dto.lines).iter().all(|c| !c.contains("apt")),
            "{dto:?}"
        );
        assert_eq!(commands(&dto.lines), vec!["dpkg -s agent-deck"]);
        state.mark_unfinished(SelfCopy::App, &error);

        assert_eq!(
            state.claim(SelfCopy::App, id).err().as_deref(),
            Some(INSTALL_MAY_BE_RUNNING)
        );
        assert!(state.claim(SelfCopy::Cli, id).is_ok());
        let answer = state.answer(id, &checked);
        assert!(!answer.app.actionable);
        assert_eq!(answer.app.confirm_question, None);
        assert_eq!(
            answer.app.lines.last().map(|line| line.text.as_str()),
            Some(INSTALL_MAY_BE_RUNNING)
        );
        assert!(answer.cli.as_ref().is_some_and(|cli| cli.actionable));

        state.mark_unfinished(SelfCopy::Cli, &unfinished(false));
        assert!(state.claim(SelfCopy::Cli, id).is_ok());
    }
}
