//! The TUI's upgrade dialog (issue #1635): the plans [`crate::self_upgrade`]
//! makes for this machine's copies, shown in the core's own words, and each
//! actionable one offered behind its own confirmation.
//!
//! It follows the desktop app's dialog (`desktop/src/components/
//! SelfUpgradeDialog.tsx`) step for step, so a user moving between the two
//! clients meets the same flow (CLAUDE.md rule 22): every copy's plan is
//! shown; the copies that can be upgraded from here are offered one at a time,
//! this TUI's own copy first; nothing changes until the user picks Upgrade;
//! Cancel before anything ran closes the dialog, and after an upgrade it skips
//! that copy; a copy that cannot be upgraded from here shows what to do and
//! offers only Close. Both start each question on Cancel, so Enter alone
//! never upgrades anything. A plan too long for the terminal scrolls, and
//! Upgrade waits until all of it has been on screen. A terminal too small to
//! show any of the plan gets a request for a larger one and only Close.
//!
//! Nothing here touches the network or a subprocess on the render thread:
//! [`check`] and the upgrade itself run on tokio tasks, and the dialog is a
//! plain state machine the TUI feeds keys and results into.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::self_upgrade::{
    CopyKind, Outcome, PlanAction, PlanLine, PlanOptions, ReleaseSource, SystemHost, UpgradeError,
    UpgradePlan, detect, discover, plan,
};
use crate::state::SharedState;

/// What the last check found: this TUI's copy first, then the other copy on
/// the machine (the desktop app) when one was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeCheck {
    pub plans: Vec<UpgradePlan>,
}

impl UpgradeCheck {
    /// The badge's text: the headline of the first copy that is behind, the
    /// same words the desktop app's notice uses. `None` while every copy is
    /// current.
    pub fn notice(&self) -> Option<String> {
        self.notice_after_install(false)
    }

    /// As [`Self::notice`], but passing over this TUI's own copy (the first
    /// plan) when `tui_installed`: its newer build is already on disk and
    /// only a restart is left.
    pub fn notice_after_install(&self, tui_installed: bool) -> Option<String> {
        self.plans
            .iter()
            .enumerate()
            .find(|(i, plan)| !(tui_installed && *i == 0) && plan.action != PlanAction::UpToDate)
            .map(|(_, plan)| plan.headline())
    }
}

/// The badge as the footer draws it: the notice and the key that opens the
/// dialog. The notice carries a release's version, which is filtered as the
/// dialog's lines are.
pub fn badge_text(notice: &str, key: &str) -> String {
    let notice = crate::untrusted_text::strip_control_and_bidi(notice, false);
    format!(" {notice} · {key} to upgrade ")
}

/// The footer's badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub text: String,
    /// Whether it offers an upgrade; `false` for the restart hint.
    pub offers_upgrade: bool,
}

/// The badge for the last `check`: the first copy that is behind, and `key`,
/// the key that opens the dialog. Once this TUI's own copy has been installed
/// on disk (`installed`, an upgrade with a [`RunResult::tui_restart`] line),
/// it is no longer offered; when nothing else is behind, the badge is that
/// restart line instead. `None` when there is nothing to say.
pub fn badge(
    check: Option<&UpgradeCheck>,
    installed: Option<&RunResult>,
    key: &str,
) -> Option<Badge> {
    let restart = installed.and_then(|result| result.tui_restart.as_ref());
    match check.and_then(|check| check.notice_after_install(restart.is_some())) {
        Some(notice) => Some(Badge {
            text: badge_text(&notice, key),
            offers_upgrade: true,
        }),
        None => restart.map(|line| Badge {
            text: format!(
                " {} ",
                crate::untrusted_text::strip_control_and_bidi(line.render().trim(), false)
            ),
            offers_upgrade: false,
        }),
    }
}

/// Check for a newer release and plan this machine's copies against it: this
/// TUI's copy, and the desktop app when one is installed. Detection and
/// planning run subprocesses (`dpkg-query`, `gh auth status`, …), so they run
/// on blocking threads.
pub async fn check() -> Result<UpgradeCheck, UpgradeError> {
    let joined = |e: tokio::task::JoinError| UpgradeError::Io(e.to_string());
    let (running, other) = tokio::task::spawn_blocking(|| {
        let host = SystemHost::default();
        let running = detect::running(&host, CopyKind::Cli)?;
        let other = match discover::other_copy(&host, &running, None) {
            discover::OtherCopy::Found(other) => Some(*other),
            discover::OtherCopy::NotFound | discover::OtherCopy::NotOffered => None,
        };
        Ok::<_, UpgradeError>((running, other))
    })
    .await
    .map_err(joined)??;
    let releases = ReleaseSource::from_build()
        .releases_for(&running, other.as_ref())
        .await?;
    let plans = tokio::task::spawn_blocking(move || {
        let options = PlanOptions::terminal(&SystemHost::default());
        std::iter::once(&running)
            .chain(other.as_ref())
            .map(|copy| plan::plan(copy, &releases, &options))
            .collect()
    })
    .await
    .map_err(joined)?;
    Ok(UpgradeCheck { plans })
}

/// Check again and, when the check could be made, publish it in `state` for
/// the badge and the dialog. A check that could not be made keeps what the
/// last one found.
pub async fn refresh(state: &SharedState) {
    if !checks_enabled() {
        return;
    }
    if let Ok(found) = check().await {
        state.write().await.upgrade_check = Some(Arc::new(found));
    }
}

/// Check at start, then every [`recheck_interval`] for as long as the TUI
/// runs, as the desktop app does.
pub async fn run_checker(state: SharedState) {
    if !checks_enabled() {
        return;
    }
    loop {
        refresh(&state).await;
        tokio::time::sleep(recheck_interval()).await;
    }
}

/// Upgrade `plan` the way the user confirmed it, off the render thread, and
/// say what happened in the core's words. `this_tui` is whether the copy is
/// the one this TUI runs from.
pub async fn run(plan: UpgradePlan, this_tui: bool) -> RunResult {
    let handle = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        let outcome = handle.block_on(crate::self_upgrade::execute::execute(
            &SystemHost::default(),
            &plan,
            &ReleaseSource::from_build(),
            &PlanOptions::default_staging_root(),
        ));
        match outcome {
            Ok(outcome) => RunResult::from_outcome(&plan, &outcome, this_tui),
            Err(error) => RunResult::from_error(&error),
        }
    })
    .await;
    result.unwrap_or_else(|e| RunResult::from_error(&UpgradeError::Io(e.to_string())))
}

/// Whether this build checks for releases. Always, except that an `e2e` build
/// checks only when a test points it at a fake release server: the L2 harness
/// runs the real binary on machines with network access, and a badge that
/// appeared whenever GitHub answered covered the right end of the footer that
/// other tests read (`visibility_001` on PR #1617's CI).
#[cfg(feature = "e2e")]
pub fn checks_enabled() -> bool {
    std::env::var_os("DOT_AGENT_DECK_TEST_RELEASES_API_URL").is_some_and(|url| !url.is_empty())
}

/// Whether this build checks for releases: always.
#[cfg(not(feature = "e2e"))]
pub fn checks_enabled() -> bool {
    true
}

/// How long the TUI waits between checks: the desktop app's interval
/// ([`crate::self_upgrade::UPDATE_RECHECK_INTERVAL`]). Under the `e2e` feature
/// only, `DOT_AGENT_DECK_TEST_UPDATE_RECHECK_SECS` shortens it, so an L2 test
/// can watch a release published while the TUI runs get noticed.
pub fn recheck_interval() -> Duration {
    #[cfg(feature = "e2e")]
    if let Some(secs) = std::env::var("DOT_AGENT_DECK_TEST_UPDATE_RECHECK_SECS")
        .ok()
        .and_then(|secs| secs.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
    {
        return Duration::from_secs(secs);
    }
    crate::self_upgrade::UPDATE_RECHECK_INTERVAL
}

/// A button in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeChoice {
    Cancel,
    Upgrade,
    Close,
}

/// Which step the dialog is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Asking whether to upgrade the plan at this index.
    Confirm(usize),
    /// The plan at this index is being carried out.
    Running(usize),
    /// Nothing is left to ask.
    Done,
}

/// What the TUI must do after a key or a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    Close,
    /// Carry out the plan at this index, off the render thread ([`run`]).
    Run(usize),
}

/// What one upgrade did, in the core's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub ok: bool,
    pub lines: Vec<PlanLine>,
    /// Set when this TUI's own copy now has the newer build on disk
    /// (replaced, installed, or upgraded by Homebrew): the core's line saying
    /// to restart. `None` for a staged copy, whose command the user has still
    /// to run, and for the desktop app.
    pub tui_restart: Option<PlanLine>,
}

impl RunResult {
    /// A finished upgrade of `plan`. When the copy is the one this TUI runs
    /// from (`this_tui`), the core's restart line follows the outcome.
    pub fn from_outcome(plan: &UpgradePlan, outcome: &Outcome, this_tui: bool) -> Self {
        let mut lines = outcome.items();
        let restart = if this_tui {
            outcome.tui_restart_line(&plan.latest)
        } else {
            None
        };
        lines.extend(restart.clone());
        let on_disk = matches!(
            outcome,
            Outcome::Replaced { .. } | Outcome::Installed { .. } | Outcome::BrewUpgraded { .. }
        );
        Self {
            ok: outcome.upgraded(),
            lines,
            tui_restart: restart.filter(|_| on_disk),
        }
    }

    /// A failed upgrade: the error, then what the core says to do instead.
    pub fn from_error(error: &UpgradeError) -> Self {
        let mut lines = vec![PlanLine::Text(error.to_string())];
        lines.extend(error.fallback());
        Self {
            ok: false,
            lines,
            tui_restart: None,
        }
    }
}

/// Where the next draw puts the view, set when the dialog moves to a new step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// The start of a copy's section: its plan's headline.
    Section(usize),
    /// The start of a copy's result.
    Result(usize),
}

/// How much of one copy's section has been on screen, in rows counted from
/// the section's first row, at the width the section was laid out for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    width: usize,
    rows: usize,
    from: usize,
    to: usize,
}

impl Seen {
    fn whole(&self) -> bool {
        self.from == 0 && self.to >= self.rows
    }
}

/// The dialog's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeDialog {
    plans: Vec<UpgradePlan>,
    results: Vec<Option<RunResult>>,
    /// Copies already answered: upgraded, failed or skipped.
    answered: Vec<bool>,
    running: Option<usize>,
    /// Whether an upgrade was started from this dialog.
    ran: bool,
    selected: UpgradeChoice,
    /// The first body row on screen.
    scroll: usize,
    /// How far the body scrolls and how many rows a page is, as of the last
    /// draw. Both zero before the first.
    max_scroll: usize,
    page: usize,
    anchor: Option<Anchor>,
    /// Per copy, what of its section has been on screen while it was asked
    /// about. Upgrade waits until all of it has.
    seen: Vec<Option<Seen>>,
    /// Whether the last draw found the terminal too small to show the plan
    /// and asked for a larger one instead. While it is, only Close is offered.
    too_small: bool,
}

impl UpgradeDialog {
    /// A dialog over `plans`, this TUI's own copy first.
    pub fn new(plans: Vec<UpgradePlan>) -> Self {
        let n = plans.len();
        let mut dialog = Self {
            plans,
            results: vec![None; n],
            answered: vec![false; n],
            running: None,
            ran: false,
            selected: UpgradeChoice::Cancel,
            scroll: 0,
            max_scroll: 0,
            page: 0,
            anchor: None,
            seen: vec![None; n],
            too_small: false,
        };
        dialog.step(None);
        dialog
    }

    /// The dialog for a TUI whose own copy (the plan at `index`) was already
    /// installed on disk this session: that copy shows `result` and is not
    /// offered again. A result with no [`RunResult::tui_restart`] line (a
    /// staged copy) leaves the offer as it is.
    pub fn with_installed(mut self, index: usize, result: RunResult) -> Self {
        if result.tui_restart.is_some()
            && let Some(slot) = self.results.get_mut(index)
        {
            *slot = Some(result);
            self.answered[index] = true;
            self.step(Some(index));
        }
        self
    }

    pub fn plans(&self) -> &[UpgradePlan] {
        &self.plans
    }

    /// The release the dialog offers, without a leading `v`.
    pub fn latest(&self) -> &str {
        self.plans.first().map_or("", |plan| plan.latest.as_str())
    }

    /// The next copy to ask about: the first actionable one not yet answered.
    fn next_offer(&self) -> Option<usize> {
        (0..self.plans.len()).find(|&i| self.plans[i].is_actionable() && !self.answered[i])
    }

    pub fn phase(&self) -> Phase {
        match (self.running, self.next_offer()) {
            (Some(i), _) => Phase::Running(i),
            (None, Some(i)) => Phase::Confirm(i),
            (None, None) => Phase::Done,
        }
    }

    /// Point the next draw at what the new step is about: the start of the
    /// copy now asked about, or, at the end, the result of the copy that just
    /// finished (`finished`).
    fn step(&mut self, finished: Option<usize>) {
        match self.phase() {
            Phase::Confirm(i) => self.anchor = Some(Anchor::Section(i)),
            Phase::Done => {
                if let Some(i) = finished {
                    self.anchor = Some(Anchor::Result(i));
                }
            }
            Phase::Running(_) => {}
        }
    }

    /// The first body row on screen.
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Scroll the body by `rows`, up when negative, within what the last draw
    /// laid out. While the terminal is too small to show the plan there is
    /// nothing to scroll.
    pub fn scroll_by(&mut self, rows: isize) {
        if self.too_small {
            return;
        }
        self.anchor = None;
        self.scroll = self.scroll.saturating_add_signed(rows).min(self.max_scroll);
    }

    /// Record whether this frame is too small to show the plan. Going into
    /// that state puts the selection back on Cancel, so a dialog drawn larger
    /// again never comes back with Upgrade chosen: it has to be chosen again.
    /// What of the plan was seen is kept.
    fn set_too_small(&mut self, too_small: bool) {
        if too_small && !self.too_small {
            self.selected = UpgradeChoice::Cancel;
        }
        self.too_small = too_small;
    }

    fn page_rows(&self) -> isize {
        isize::try_from(self.page.max(1)).unwrap_or(isize::MAX)
    }

    /// Whether the whole of copy `index`'s section, its command and its
    /// provenance line included, has been on screen.
    fn section_seen(&self, index: usize) -> bool {
        self.seen
            .get(index)
            .copied()
            .flatten()
            .is_some_and(|seen| seen.whole())
    }

    /// Whether an upgrade is under way.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether any upgrade has run or is running in this dialog.
    pub fn ran_any(&self) -> bool {
        self.ran || self.running.is_some()
    }

    /// The highlighted button.
    pub fn selected(&self) -> UpgradeChoice {
        match self.phase() {
            Phase::Done => UpgradeChoice::Close,
            Phase::Confirm(_) if self.too_small => UpgradeChoice::Close,
            _ => self.selected,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        if self.too_small {
            return match (self.phase(), key.code) {
                (Phase::Running(_), _) => Effect::None,
                (_, KeyCode::Enter | KeyCode::Esc) => Effect::Close,
                _ => Effect::None,
            };
        }
        match key.code {
            KeyCode::PageDown => {
                self.scroll_by(self.page_rows());
                return Effect::None;
            }
            KeyCode::PageUp => {
                self.scroll_by(-self.page_rows());
                return Effect::None;
            }
            _ => {}
        }
        match self.phase() {
            Phase::Running(_) => Effect::None,
            Phase::Confirm(_) => match key.code {
                KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h') => {
                    self.selected = UpgradeChoice::Cancel;
                    Effect::None
                }
                KeyCode::Down | KeyCode::Right | KeyCode::Char('j') | KeyCode::Char('l') => {
                    self.selected = UpgradeChoice::Upgrade;
                    Effect::None
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    self.selected = match self.selected {
                        UpgradeChoice::Upgrade => UpgradeChoice::Cancel,
                        _ => UpgradeChoice::Upgrade,
                    };
                    Effect::None
                }
                KeyCode::Enter => self.choose(self.selected),
                KeyCode::Esc => Effect::Close,
                _ => Effect::None,
            },
            Phase::Done => match key.code {
                KeyCode::Enter | KeyCode::Esc => Effect::Close,
                _ => Effect::None,
            },
        }
    }

    /// Press `choice`, by key or by click. Upgrade waits until the whole of
    /// the copy's plan has been on screen: until then it scrolls on instead.
    /// In a terminal too small to show the plan, only Close does anything.
    pub fn choose(&mut self, choice: UpgradeChoice) -> Effect {
        match self.phase() {
            Phase::Running(_) => Effect::None,
            _ if self.too_small => match choice {
                UpgradeChoice::Upgrade => Effect::None,
                UpgradeChoice::Cancel | UpgradeChoice::Close => Effect::Close,
            },
            Phase::Confirm(i) => match choice {
                UpgradeChoice::Upgrade if !self.section_seen(i) => {
                    self.scroll_by(self.page_rows());
                    Effect::None
                }
                UpgradeChoice::Upgrade => {
                    self.running = Some(i);
                    self.ran = true;
                    Effect::Run(i)
                }
                UpgradeChoice::Cancel if self.ran_any() => {
                    self.answered[i] = true;
                    self.selected = UpgradeChoice::Cancel;
                    self.step(None);
                    Effect::None
                }
                UpgradeChoice::Cancel | UpgradeChoice::Close => Effect::Close,
            },
            Phase::Done => match choice {
                UpgradeChoice::Upgrade => Effect::None,
                UpgradeChoice::Cancel | UpgradeChoice::Close => Effect::Close,
            },
        }
    }

    /// The upgrade of the plan at `index` finished with `result`.
    pub fn finish(&mut self, index: usize, result: RunResult) {
        self.ran = true;
        if let Some(slot) = self.results.get_mut(index) {
            *slot = Some(result);
            self.answered[index] = true;
        }
        if self.running == Some(index) {
            self.running = None;
        }
        self.selected = UpgradeChoice::Cancel;
        self.step(Some(index));
    }

    /// Record what the last draw put on screen: the body's extent and page,
    /// where each section starts and ends, and so how much of the section
    /// being asked about has now been seen.
    fn place(&mut self, layout: &Layout, room: usize, width: usize) {
        self.max_scroll = layout.rows.saturating_sub(room);
        self.page = room.saturating_sub(1).max(1);
        if let Some(anchor) = self.anchor.take() {
            self.scroll = match anchor {
                Anchor::Section(i) => layout.sections.get(i).map_or(0, |s| s.0),
                Anchor::Result(i) => layout.results.get(i).copied().flatten().unwrap_or(0),
            };
        }
        self.scroll = self.scroll.min(self.max_scroll);
        let Phase::Confirm(i) = self.phase() else {
            return;
        };
        let Some(&(start, end)) = layout.sections.get(i) else {
            return;
        };
        let rows = end - start;
        let from = self.scroll.max(start);
        let to = (self.scroll + room).min(end);
        if from >= to {
            return;
        }
        let (from, to) = (from - start, to - start);
        let seen = &mut self.seen[i];
        *seen = match *seen {
            Some(was)
                if was.width == width && was.rows == rows && from <= was.to && to >= was.from =>
            {
                Some(Seen {
                    from: was.from.min(from),
                    to: was.to.max(to),
                    ..was
                })
            }
            Some(was) if was.width == width && was.rows == rows => Some(was),
            _ => Some(Seen {
                width,
                rows,
                from,
                to,
            }),
        };
    }
}

/// Where the body's parts fall, in body rows.
struct Layout {
    rows: usize,
    /// Each copy's section: from its headline to its last row.
    sections: Vec<(usize, usize)>,
    /// The first row of each copy's result, once it has one.
    results: Vec<Option<usize>>,
}

/// The widest the dialog grows: wide enough that the core's longest sentence
/// (a provenance line) fits on one row.
const MAX_WIDTH: u16 = 160;

/// The narrowest the plan's text is drawn: wide enough for the widest row the
/// dialog never wraps (a hint, a button), which also leaves a command its
/// two-column indent and room for any character.
const MIN_TEXT_WIDTH: u16 = 60;

/// The narrowest terminal the dialog shows a plan in: [`MIN_TEXT_WIDTH`], the
/// dialog's border and padding, and the margin either side of it. Narrower,
/// it asks for a larger terminal instead.
pub const MIN_COLUMNS: u16 = MIN_TEXT_WIDTH + 8;

fn plain() -> Style {
    Style::default().fg(Color::Reset)
}

fn bold() -> Style {
    plain().add_modifier(Modifier::BOLD)
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// `text` wrapped to `width` display columns, otherwise verbatim: a row ends
/// at the last single space that fits, and that one space is dropped, so the
/// rows joined with a space are `text` again. A run of spaces is never a break
/// (the core's checksum command carries two spaces between a hash and a path,
/// and they must reach the screen as two). A stretch with no such space that
/// is wider than a row is broken at the row's edge.
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut rest = text;
    while rest.width() > width {
        let chars: Vec<(usize, char)> = rest.char_indices().collect();
        let mut used = 0;
        let mut cut = None;
        let mut edge = rest.len();
        for (n, &(i, c)) in chars.iter().enumerate() {
            let single = c == ' '
                && n > 0
                && chars[n - 1].1 != ' '
                && chars.get(n + 1).is_some_and(|&(_, next)| next != ' ');
            if single && used <= width {
                cut = Some(i);
            }
            used += c.width().unwrap_or(0);
            if used > width {
                edge = if i == 0 { c.len_utf8() } else { i };
                break;
            }
        }
        match cut {
            Some(i) => {
                rows.push(rest[..i].to_string());
                rest = &rest[i + 1..];
            }
            None => {
                rows.push(rest[..edge].to_string());
                rest = &rest[edge..];
            }
        }
    }
    rows.push(rest.to_string());
    rows
}

/// Push `line` wrapped to `width` onto `out`: prose as it is, a command
/// indented by two columns and coloured, so it reads as something to run.
fn push_line(out: &mut Vec<Line<'static>>, line: &PlanLine, width: usize, style: Style) {
    let (indent, text, style) = match line {
        PlanLine::Text(text) => ("", text.as_str(), style),
        PlanLine::Command(command) => ("  ", command.as_str(), Style::default().fg(Color::Cyan)),
    };
    let clean = crate::untrusted_text::strip_control_and_bidi(text, false);
    for row in wrap(&clean, width.saturating_sub(indent.len())) {
        out.push(Line::styled(format!("{indent}{row}"), style));
    }
}

/// The hints on the dialog's last row, one per step. They are never wrapped,
/// so [`MIN_TEXT_WIDTH`] is at least as wide as the widest.
const HINT_READ: &str = "PageDown to read the whole plan · ↑/↓ choose · Esc closes";
const HINT_CONFIRM_SCROLL: &str = "↑/↓ choose · Enter confirms · PgUp/PgDn scroll · Esc closes";
const HINT_CONFIRM: &str = "↑/↓ choose · Enter confirms · Esc closes";
const HINT_RUNNING: &str = "Upgrading… wait for it to finish.";
const HINT_DONE_SCROLL: &str = "PgUp/PgDn scroll · Enter or Esc closes";
const HINT_DONE: &str = "Enter or Esc closes";

/// The buttons for `phase`, each with its label. They are never wrapped
/// either.
fn buttons(phase: Phase, ran_any: bool) -> Vec<(UpgradeChoice, String)> {
    match phase {
        Phase::Confirm(_) => {
            let cancel = if ran_any {
                "skip this copy"
            } else {
                "close, changing nothing"
            };
            vec![
                (UpgradeChoice::Cancel, format!("Cancel   — {cancel}")),
                (
                    UpgradeChoice::Upgrade,
                    "Upgrade  — upgrade it now".to_string(),
                ),
            ]
        }
        Phase::Running(_) => Vec::new(),
        Phase::Done => vec![(UpgradeChoice::Close, "Close".to_string())],
    }
}

/// The question, a blank row and the buttons, always kept in view, laid out
/// for `width` columns. The first row is where the "more below" marker goes;
/// the hint, the last row, is added once the view is placed. Returns the
/// rows, the buttons and the row of the first button.
fn footer_rows(
    dialog: &UpgradeDialog,
    phase: Phase,
    width: usize,
) -> (Vec<Line<'static>>, Vec<(UpgradeChoice, String)>, usize) {
    let mut footer: Vec<Line<'static>> = vec![Line::from("")];
    if let Phase::Confirm(i) = phase {
        let question = dialog.plans[i].confirm_question().unwrap_or_default();
        let question = crate::untrusted_text::strip_control_and_bidi(&question, false);
        for row in wrap(&question, width) {
            footer.push(Line::styled(row, bold()));
        }
        footer.push(Line::from(""));
    }
    let buttons = buttons(phase, dialog.ran_any());
    let first_button_row = footer.len();
    let selected = dialog.selected();
    for (choice, label) in &buttons {
        let (cursor, style) = if *choice == selected {
            (">", selected_style())
        } else {
            (" ", plain())
        };
        footer.push(Line::styled(format!("{cursor} {label}"), style));
    }
    footer.push(Line::from(""));
    (footer, buttons, first_button_row)
}

/// Where the text goes in a popup at `popup`: inside `block`'s border, with
/// one column of padding on each side.
fn text_rect(block: &Block, popup: Rect) -> Rect {
    let inner = block.inner(popup);
    Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    )
}

/// Draw `dialog` centred over the frame. Returns each button's row, for the
/// mouse.
///
/// The body scrolls: when it does not fit, a marker above or below it says
/// there is more, PageUp and PageDown (or the mouse wheel) move through it,
/// and the question and the buttons stay in view. Nothing is cut off. Drawing
/// records what reached the screen, which is what lets Upgrade be chosen.
///
/// The plan is laid out for the text area it is drawn into. When that area is
/// narrower than [`MIN_TEXT_WIDTH`] or has no row left for the plan, the
/// dialog asks for a larger terminal instead ([`render_too_small`]), and
/// nothing counts as seen.
pub fn render(frame: &mut Frame, dialog: &mut UpgradeDialog) -> Vec<(UpgradeChoice, Rect)> {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    let max_height = area.height.saturating_sub(2);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let title = crate::untrusted_text::strip_control_and_bidi(dialog.latest(), false);
    let block = Block::default()
        .title(format!(" Upgrade to v{title} "))
        .title_style(selected_style())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    // The text area of the tallest popup the frame allows. A shorter plan
    // draws a shorter popup, as wide.
    let most = text_rect(&block, Rect::new(x, area.y, width, max_height));
    let text_width = usize::from(most.width);
    let phase = dialog.phase();

    // This frame's size decides what is selected, so it is settled before
    // anything that draws the selection. The footer's rows do not depend on
    // the selection, only on the width.
    let footer_len = footer_rows(dialog, phase, text_width).0.len();
    // The "more above" row, the footer and its hint leave the body the rest.
    let room = usize::from(most.height).saturating_sub(1 + footer_len + 1);
    dialog.set_too_small(most.width < MIN_TEXT_WIDTH || room == 0);
    if dialog.too_small {
        // The footer as it would be at the narrowest width that shows a plan,
        // when this one is narrower, so the size asked for is enough.
        let rows = if most.width < MIN_TEXT_WIDTH {
            footer_rows(dialog, phase, usize::from(MIN_TEXT_WIDTH))
                .0
                .len()
        } else {
            footer_len
        };
        // One body row, the marker row and the hint, the border, the margin.
        let needed = u16::try_from(rows + 1 + 2 + 2 + 2).unwrap_or(u16::MAX);
        return render_too_small(frame, phase, &title, needed);
    }
    let (mut footer, buttons, first_button_row) = footer_rows(dialog, phase, text_width);

    let mut body: Vec<Line<'static>> = Vec::new();
    let mut layout = Layout {
        rows: 0,
        sections: Vec::new(),
        results: Vec::new(),
    };
    for (i, plan) in dialog.plans.iter().enumerate() {
        if i > 0 {
            body.push(Line::from(""));
        }
        let start = body.len();
        let items = plan.items();
        if let Some((headline, rest)) = items.split_first() {
            push_line(&mut body, headline, text_width, bold());
            for line in rest {
                push_line(&mut body, line, text_width, plain());
            }
        }
        if phase == Phase::Running(i) {
            body.push(Line::styled("Upgrading…", selected_style()));
        }
        let mut result_start = None;
        if let Some(result) = &dialog.results[i] {
            result_start = Some(body.len());
            let style = Style::default().fg(if result.ok { Color::Green } else { Color::Red });
            for line in &result.lines {
                push_line(&mut body, line, text_width, style);
            }
        }
        layout.sections.push((start, body.len()));
        layout.results.push(result_start);
    }
    layout.rows = body.len();

    dialog.place(&layout, room, text_width);
    let scroll = dialog.scroll;
    let scrolls = dialog.max_scroll > 0;
    let hint = match phase {
        Phase::Confirm(i) if !dialog.section_seen(i) => HINT_READ,
        Phase::Confirm(_) if scrolls => HINT_CONFIRM_SCROLL,
        Phase::Confirm(_) => HINT_CONFIRM,
        Phase::Running(_) => HINT_RUNNING,
        Phase::Done if scrolls => HINT_DONE_SCROLL,
        Phase::Done => HINT_DONE,
    };
    footer.push(Line::styled(hint, plain().add_modifier(Modifier::DIM)));
    let marker = |text: &'static str| Line::styled(text, Style::default().fg(Color::Yellow));
    if scroll + room < body.len() {
        footer[0] = marker("↓ more — PgDn");
    }

    let visible: Vec<Line<'static>> = body.into_iter().skip(scroll).take(room).collect();
    let mut text = vec![if scroll > 0 {
        marker("↑ more — PgUp")
    } else {
        Line::from("")
    }];
    let body_rows = visible.len();
    text.extend(visible);
    text.extend(footer);
    let height = u16::try_from(text.len() + 2)
        .unwrap_or(u16::MAX)
        .min(max_height);

    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    let padded = text_rect(&block, popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), padded);

    // Rows of the buttons: the marker row, the body, then the footer.
    let top = 1 + body_rows + first_button_row;
    buttons
        .iter()
        .enumerate()
        .filter_map(|(n, (choice, _))| {
            let row = u16::try_from(top + n).ok()?;
            (row < padded.height).then(|| {
                (
                    *choice,
                    Rect::new(padded.x, padded.y + row, padded.width, 1),
                )
            })
        })
        .collect()
}

/// Push `text` wrapped to `width` onto `out`.
fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for row in wrap(text, width) {
        out.push(Line::styled(row, style));
    }
}

/// The dialog in a terminal too small to show the plan: what size it needs
/// (`rows` tall, [`MIN_COLUMNS`] wide), and Close. While an upgrade runs there
/// is nothing to close, as in the full dialog. Drawn over the whole frame,
/// wrapped to it.
fn render_too_small(
    frame: &mut Frame,
    phase: Phase,
    title: &str,
    rows: u16,
) -> Vec<(UpgradeChoice, Rect)> {
    let area = frame.area();
    let width = usize::from(area.width);
    let mut text: Vec<Line<'static>> = Vec::new();
    push_wrapped(
        &mut text,
        &format!("Upgrade to v{title}"),
        width,
        selected_style(),
    );
    text.push(Line::from(""));
    push_wrapped(
        &mut text,
        "The terminal is too small to show the upgrade plan.",
        width,
        plain(),
    );
    push_wrapped(
        &mut text,
        &format!("Make it at least {MIN_COLUMNS} columns wide and {rows} rows tall."),
        width,
        plain(),
    );
    text.push(Line::from(""));
    let dim = plain().add_modifier(Modifier::DIM);
    let close_row = match phase {
        Phase::Running(_) => {
            push_wrapped(&mut text, HINT_RUNNING, width, dim);
            None
        }
        Phase::Confirm(_) | Phase::Done => {
            let row = text.len();
            push_wrapped(&mut text, "> Close", width, selected_style());
            text.push(Line::from(""));
            push_wrapped(&mut text, HINT_DONE, width, dim);
            Some(row)
        }
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(text), area);
    close_row
        .and_then(|row| u16::try_from(row).ok())
        .filter(|&row| row < area.height)
        .map(|row| {
            (
                UpgradeChoice::Close,
                Rect::new(area.x, area.y + row, area.width, 1),
            )
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_at_spaces_and_splits_overlong_words() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), vec![""]);
        assert_eq!(wrap("a bcdefgh", 4), vec!["a", "bcde", "fgh"]);
        // Two spaces are kept, and never broken at.
        assert_eq!(wrap("x 'ab  cd' y", 9), vec!["x", "'ab  cd'", "y"]);
    }

    #[test]
    fn a_brew_upgrade_that_did_not_move_is_not_installed() {
        use crate::self_upgrade::detect::{InstallMethod, Installation, Platform, Tools};
        use crate::self_upgrade::{CopyKind, HomebrewFormula, PlanOptions, ProvenanceCheck};
        let installation = Installation {
            copy: CopyKind::Cli,
            executable: "/opt/homebrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck".into(),
            version: "0.46.0".into(),
            platform: Some(Platform::MacosArm64),
            method: InstallMethod::Homebrew {
                formula: HomebrewFormula::Stable,
                prefix: "/opt/homebrew".into(),
            },
            tools: Tools {
                brew: Some("/opt/homebrew/bin/brew".into()),
                ..Tools::default()
            },
        };
        let plan = plan::plan(
            &installation,
            &"0.47.0".into(),
            &PlanOptions {
                staging_root: "/stage".into(),
                can_prompt_for_privilege: false,
                provenance: ProvenanceCheck::Unavailable { reason: "x".into() },
            },
        );
        let behind = Outcome::BrewNotUpgraded {
            formula: "dot-agent-deck",
            reported: Some("0.46.0".into()),
            offered: "0.47.0".into(),
        };
        let result = RunResult::from_outcome(&plan, &behind, true);
        assert!(!result.ok);
        assert_eq!(result.tui_restart, None, "nothing to restart into");
        assert!(
            !result
                .lines
                .iter()
                .any(|line| line.render().contains("start it again")),
            "{result:?}"
        );
        // So the badge still offers the upgrade.
        let check = UpgradeCheck {
            plans: vec![plan.clone()],
        };
        let badge = badge(Some(&check), Some(&result), "u").unwrap();
        assert!(badge.offers_upgrade);

        let reached = Outcome::BrewUpgraded {
            formula: "dot-agent-deck",
            reported: Some("0.47.0".into()),
        };
        let result = RunResult::from_outcome(&plan, &reached, true);
        assert!(result.ok);
        assert!(result.tui_restart.is_some());
    }

    #[test]
    fn rows_never_wrapped_fit_the_narrowest_text_width() {
        let min = usize::from(MIN_TEXT_WIDTH);
        let hints = [
            HINT_READ,
            HINT_CONFIRM_SCROLL,
            HINT_CONFIRM,
            HINT_RUNNING,
            HINT_DONE_SCROLL,
            HINT_DONE,
        ];
        for hint in hints {
            assert!(hint.width() <= min, "{hint:?}");
        }
        for phase in [Phase::Confirm(0), Phase::Running(0), Phase::Done] {
            for ran_any in [false, true] {
                for (_, label) in buttons(phase, ran_any) {
                    assert!(format!("> {label}").width() <= min, "{label:?}");
                }
            }
        }
        // A command's indent leaves room for a character two columns wide.
        assert!(min >= 2 + 2);
    }
}
