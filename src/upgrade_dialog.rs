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
//! offers only Close. The one deliberate difference is the default: the TUI's
//! confirmation starts on Cancel, so Enter alone never upgrades anything.
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
        self.plans
            .iter()
            .find(|plan| plan.action != PlanAction::UpToDate)
            .map(UpgradePlan::headline)
    }
}

/// The badge as the footer draws it: the notice and the key that opens the
/// dialog.
pub fn badge_text(notice: &str, key: &str) -> String {
    format!(" {notice} · {key} to upgrade ")
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
}

impl RunResult {
    /// A finished upgrade of `plan`. When the copy is the one this TUI runs
    /// from (`this_tui`), the core's restart line follows the outcome.
    pub fn from_outcome(plan: &UpgradePlan, outcome: &Outcome, this_tui: bool) -> Self {
        let mut lines = outcome.items();
        if this_tui {
            lines.extend(outcome.tui_restart_line(&plan.latest));
        }
        Self { ok: true, lines }
    }

    /// A failed upgrade: the error, then what the core says to do instead.
    pub fn from_error(error: &UpgradeError) -> Self {
        let mut lines = vec![PlanLine::Text(error.to_string())];
        lines.extend(error.fallback());
        Self { ok: false, lines }
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
    selected: UpgradeChoice,
}

impl UpgradeDialog {
    /// A dialog over `plans`, this TUI's own copy first.
    pub fn new(plans: Vec<UpgradePlan>) -> Self {
        let n = plans.len();
        Self {
            plans,
            results: vec![None; n],
            answered: vec![false; n],
            running: None,
            selected: UpgradeChoice::Cancel,
        }
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

    /// Whether an upgrade is under way.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether any upgrade has run or is running.
    pub fn ran_any(&self) -> bool {
        self.running.is_some() || self.results.iter().any(Option::is_some)
    }

    /// The highlighted button.
    pub fn selected(&self) -> UpgradeChoice {
        match self.phase() {
            Phase::Done => UpgradeChoice::Close,
            _ => self.selected,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
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

    /// Press `choice`, by key or by click.
    pub fn choose(&mut self, choice: UpgradeChoice) -> Effect {
        match self.phase() {
            Phase::Running(_) => Effect::None,
            Phase::Confirm(i) => match choice {
                UpgradeChoice::Upgrade => {
                    self.running = Some(i);
                    Effect::Run(i)
                }
                UpgradeChoice::Cancel if self.ran_any() => {
                    self.answered[i] = true;
                    self.selected = UpgradeChoice::Cancel;
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
        if let Some(slot) = self.results.get_mut(index) {
            *slot = Some(result);
            self.answered[index] = true;
        }
        if self.running == Some(index) {
            self.running = None;
        }
        self.selected = UpgradeChoice::Cancel;
    }
}

/// The widest the dialog grows: wide enough that the core's longest sentence
/// (a provenance line) fits on one row.
const MAX_WIDTH: u16 = 160;

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

/// Draw `dialog` centred over the frame. Returns each button's row, for the
/// mouse.
pub fn render(frame: &mut Frame, dialog: &UpgradeDialog) -> Vec<(UpgradeChoice, Rect)> {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(MAX_WIDTH);
    // Inside the border, one column of padding on each side.
    let text_width = usize::from(width.saturating_sub(4));
    let phase = dialog.phase();

    let mut body: Vec<Line<'static>> = Vec::new();
    for (i, plan) in dialog.plans.iter().enumerate() {
        if i > 0 {
            body.push(Line::from(""));
        }
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
        if let Some(result) = &dialog.results[i] {
            let style = Style::default().fg(if result.ok { Color::Green } else { Color::Red });
            for line in &result.lines {
                push_line(&mut body, line, text_width, style);
            }
        }
    }

    // The question and the buttons, always kept in view.
    let mut footer: Vec<Line<'static>> = vec![Line::from("")];
    let mut buttons: Vec<(UpgradeChoice, String)> = Vec::new();
    let hint = match phase {
        Phase::Confirm(i) => {
            let question = dialog.plans[i].confirm_question().unwrap_or_default();
            for row in wrap(&question, text_width) {
                footer.push(Line::styled(row, bold()));
            }
            footer.push(Line::from(""));
            let cancel = if dialog.ran_any() {
                "skip this copy"
            } else {
                "close, changing nothing"
            };
            buttons.push((UpgradeChoice::Cancel, format!("Cancel   — {cancel}")));
            buttons.push((
                UpgradeChoice::Upgrade,
                "Upgrade  — upgrade it now".to_string(),
            ));
            "↑/↓ choose · Enter confirms · Esc closes"
        }
        Phase::Running(_) => "Upgrading… wait for it to finish.",
        Phase::Done => {
            buttons.push((UpgradeChoice::Close, "Close".to_string()));
            "Enter or Esc closes"
        }
    };
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
    footer.push(Line::styled(hint, plain().add_modifier(Modifier::DIM)));

    // Borders and a blank first row; the body gives way to the footer, from
    // the top, so the newest result and the question stay visible.
    let chrome = 3usize;
    let max_height = usize::from(area.height.saturating_sub(2));
    let room = max_height.saturating_sub(chrome + footer.len());
    if body.len() > room {
        body.drain(..body.len() - room);
    }
    let mut text = vec![Line::from("")];
    let body_rows = body.len();
    text.extend(body);
    text.extend(footer);
    let height = u16::try_from(text.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);

    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" Upgrade to v{} ", dialog.latest()))
        .title_style(selected_style())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    frame.render_widget(Paragraph::new(text), padded);

    // Rows of the buttons: the blank first row, the body, then the footer.
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
}
