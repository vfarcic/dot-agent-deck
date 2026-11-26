//! The TUI's Upgrade all daemons dialog (issue #1600): every remote deck on
//! the deck list checked, then the older ones upgraded one after another
//! through [`crate::daemon_upgrade_batch`], the batch the CLI's `remote
//! upgrade --all` and the desktop app's Upgrade all dialog run too.
//!
//! It follows the desktop app's dialog (`desktop/src/components/
//! UpgradeAllDialog.tsx`) step for step and word for word, so a user moving
//! between the two meets one flow (CLAUDE.md rule 22):
//!
//! 1. **Checking** every remote deck.
//! 2. **The plan**: each deck and what will happen to it; Cancel or Upgrade,
//!    starting on Cancel so Enter alone upgrades nothing.
//! 3. **Running**: each deck's line as it goes; Stop after this daemon.
//! 4. **The question**, only from a daemon with live agents or roles, naming
//!    its own: Keep current daemon (the default, and what Esc answers) or
//!    Restart now.
//! 5. **The results**: one line per deck, and Close.
//!
//! Nothing here blocks the render thread: the check and the run go on
//! blocking tasks ([`Job`]), and the dialog is a state machine the TUI feeds
//! keys and [`Update`]s into once a frame.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::daemon_protocol::RestartStopSet;
use crate::daemon_upgrade::{RestartChoice, RestartDecider, UpgradeStage};
use crate::daemon_upgrade_batch::{
    BatchDeck, BatchEvent, BatchPlan, BatchReport, DeckCheck, DeckReport, DeckStatus,
    RemoteBatchDeck, check_all, no_report, run_plan, stop_set_count,
};
use crate::untrusted_text::{REMOTE_MESSAGE_MAX_BYTES, display_line, display_message};

/// The dialog's title, the same words as the desktop's and the help entry's.
pub const TITLE: &str = "Upgrade all daemons";

/// What the dialog shows while every deck is checked.
pub const CHECKING: &str = "Checking every remote deck on the deck list…";

/// Where the dialog is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Every deck is being checked.
    Checking,
    /// The deck list could not be read (`started: false`), or the check or
    /// the run stopped before it reported (`started` says whether anything
    /// may have been upgraded).
    Failed { reason: String, started: bool },
    /// What the check found, waiting for Upgrade or Cancel.
    Plan(BatchPlan),
    /// The upgrades are running.
    Running(Run),
    /// Every deck has its result.
    Done(BatchReport),
}

/// The run's state: the plan, the deck being upgraded and its stage, the
/// results so far, and the question a daemon is asking, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub plan: BatchPlan,
    pub current: Option<usize>,
    pub stage: Option<UpgradeStage>,
    pub finished: Vec<Option<DeckReport>>,
    pub question: Option<Question>,
    /// Stop after this daemon was pressed.
    pub stopping: bool,
}

/// "Restarting the daemon on `deck` stops these — restart now?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub deck: String,
    pub at_stake: RestartStopSet,
    /// What would stop changed since the last time this daemon asked.
    pub stale: bool,
}

/// A button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Cancel,
    Upgrade,
    Stop,
    Keep,
    RestartNow,
    Close,
}

impl Choice {
    /// The button's words, the desktop's too.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cancel => "Cancel",
            Self::Upgrade => "Upgrade",
            Self::Stop => "Stop after this daemon",
            Self::Keep => "Keep current daemon",
            Self::RestartNow => "Restart now",
            Self::Close => "Close",
        }
    }
}

/// What a key or a click asks the TUI to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Close the dialog.
    Close,
    /// Start upgrading the plan's decks.
    Start,
    /// Start no deck after the current one.
    Stop,
    /// Answer the open question.
    Answer(RestartChoice),
}

/// What the background work reports.
pub enum Update {
    Checked(Result<BatchPlan, String>),
    Event(BatchEvent),
    /// A daemon asks; the answer goes back on the sender.
    Asked(Question, Sender<RestartChoice>),
    Finished(BatchReport),
    /// The work stopped without its final report; `started` says whether the
    /// upgrades had begun.
    Stopped {
        reason: String,
        started: bool,
    },
}

/// The dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeAllDialog {
    phase: Phase,
    /// The selected button, as an index into [`Self::buttons`].
    selected: usize,
    /// How many body rows are scrolled past.
    scroll: usize,
    /// A question just arrived: the next draw scrolls to its first row.
    scroll_to_question: bool,
    /// Every row of the open question has been on screen. Restart now is
    /// refused until it has, so nothing is stopped that was not shown.
    question_seen: bool,
}

impl Default for UpgradeAllDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl UpgradeAllDialog {
    pub fn new() -> Self {
        Self {
            phase: Phase::Checking,
            selected: 0,
            scroll: 0,
            scroll_to_question: false,
            question_seen: false,
        }
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Whether upgrades are running, so the dialog may not be closed.
    pub fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running(_))
    }

    /// The buttons the dialog shows now, in order. The first is the one
    /// selected when they appear: never the one that changes anything.
    pub fn buttons(&self) -> Vec<Choice> {
        match &self.phase {
            Phase::Checking => Vec::new(),
            Phase::Failed { .. } | Phase::Done(_) => vec![Choice::Close],
            Phase::Plan(plan) if plan.to_upgrade() == 0 => vec![Choice::Close],
            Phase::Plan(_) => vec![Choice::Cancel, Choice::Upgrade],
            Phase::Running(run) if run.question.is_some() => {
                vec![Choice::Keep, Choice::RestartNow]
            }
            Phase::Running(run) if run.stopping => Vec::new(),
            Phase::Running(_) => vec![Choice::Stop],
        }
    }

    pub fn selected(&self) -> Option<Choice> {
        self.buttons().get(self.selected).copied()
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
    }

    /// A key, while the dialog owns the keyboard.
    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        let count = self.buttons().len();
        match key.code {
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab if count > 0 => {
                self.selected = (self.selected + count - 1) % count;
                Effect::None
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab if count > 0 => {
                self.selected = (self.selected + 1) % count;
                Effect::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(5);
                Effect::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(5);
                Effect::None
            }
            KeyCode::Enter => match self.selected() {
                Some(choice) => self.choose(choice),
                None => Effect::None,
            },
            // Esc is the answer that changes nothing: Keep at a question,
            // Stop while running, Cancel or Close otherwise.
            KeyCode::Esc => match &self.phase {
                Phase::Checking => Effect::Close,
                Phase::Running(run) if run.question.is_some() => self.choose(Choice::Keep),
                Phase::Running(run) if run.stopping => Effect::None,
                Phase::Running(_) => self.choose(Choice::Stop),
                _ => self.choose(Choice::Close),
            },
            _ => Effect::None,
        }
    }

    /// A button, chosen by Enter or a click. A button the dialog does not
    /// show now does nothing.
    pub fn choose(&mut self, choice: Choice) -> Effect {
        let shown =
            self.buttons().contains(&choice) || (choice == Choice::Close && !self.is_running());
        if !shown {
            return Effect::None;
        }
        match choice {
            Choice::Cancel | Choice::Close => Effect::Close,
            Choice::Upgrade => {
                let Phase::Plan(plan) = &self.phase else {
                    return Effect::None;
                };
                let finished = vec![None; plan.decks.len()];
                self.phase = Phase::Running(Run {
                    plan: plan.clone(),
                    current: None,
                    stage: None,
                    finished,
                    question: None,
                    stopping: false,
                });
                self.reset_selection();
                Effect::Start
            }
            Choice::Stop => {
                if let Phase::Running(run) = &mut self.phase {
                    run.stopping = true;
                }
                self.reset_selection();
                Effect::Stop
            }
            // Nothing is stopped that the user could not have read.
            Choice::RestartNow if !self.question_seen => Effect::None,
            Choice::Keep | Choice::RestartNow => {
                if let Phase::Running(run) = &mut self.phase {
                    run.question = None;
                }
                self.reset_selection();
                Effect::Answer(if choice == Choice::Keep {
                    RestartChoice::KeepCurrent
                } else {
                    RestartChoice::RestartNow
                })
            }
        }
    }

    /// What the background work reported. A question arrives through
    /// [`Self::ask`], with the sender the TUI keeps for its answer.
    pub fn apply(&mut self, update: &Update) {
        match update {
            Update::Checked(Ok(plan)) => {
                self.phase = Phase::Plan(plan.clone());
                self.reset_selection();
            }
            Update::Checked(Err(reason)) => {
                self.phase = Phase::Failed {
                    reason: reason.clone(),
                    started: false,
                };
                self.reset_selection();
            }
            Update::Stopped { reason, started } => {
                self.phase = Phase::Failed {
                    reason: reason.clone(),
                    started: *started,
                };
                self.reset_selection();
            }
            Update::Event(event) => {
                if let Phase::Running(run) = &mut self.phase {
                    match event {
                        BatchEvent::Started { index, .. } => {
                            run.current = Some(*index);
                            run.stage = None;
                        }
                        BatchEvent::Progress {
                            index, progress, ..
                        } if run.current == Some(*index) => run.stage = Some(progress.stage),
                        BatchEvent::Progress { .. } => {}
                        BatchEvent::Finished { index, report } => {
                            if let Some(slot) = run.finished.get_mut(*index) {
                                *slot = Some(report.clone());
                            }
                            if run.current == Some(*index) {
                                run.current = None;
                                run.stage = None;
                            }
                        }
                    }
                }
            }
            Update::Asked(question, _) => self.ask(question.clone()),
            Update::Finished(report) => {
                self.phase = Phase::Done(report.clone());
                self.reset_selection();
            }
        }
    }

    /// A daemon asks whether to stop what it names.
    pub fn ask(&mut self, question: Question) {
        if let Phase::Running(run) = &mut self.phase {
            run.question = Some(question);
            self.selected = 0;
            self.scroll_to_question = true;
            self.question_seen = false;
        }
    }

    /// Whether Restart now can be chosen: every agent and role the question
    /// names has been on screen.
    pub fn question_seen(&self) -> bool {
        self.question_seen
    }

    /// The body: one line per deck, in deck-list order. A daemon's question,
    /// [`Self::question_lines`], is drawn below it, scrolled into view when it
    /// arrives.
    pub fn body_lines(&self) -> Vec<(String, Style)> {
        let mut lines = Vec::new();
        let deck = |name: &str| display_line(name, crate::untrusted_text::REMOTE_NAME_MAX_BYTES);
        match &self.phase {
            Phase::Checking => lines.push((CHECKING.to_string(), plain())),
            Phase::Failed { reason, started } => {
                lines.push((
                    if *started {
                        "Upgrade all stopped unexpectedly, so it is not known how far it got. `dot-agent-deck connect <name>` shows what a daemon runs now."
                    } else {
                        "Nothing was checked or upgraded."
                    }
                    .to_string(),
                    plain(),
                ));
                lines.push((
                    display_message(reason, REMOTE_MESSAGE_MAX_BYTES),
                    failure_style(),
                ));
            }
            Phase::Plan(plan) => {
                lines.push((plan_headline(plan), bold()));
                for planned in &plan.decks {
                    lines.push((
                        format!("{}: {}", deck(&planned.deck), planned.line()),
                        check_style(&planned.check),
                    ));
                }
                lines.push((String::new(), plain()));
                let n = plan.to_upgrade();
                if n == 0 {
                    lines.push(("Nothing to upgrade.".to_string(), bold()));
                } else {
                    lines.push((
                        format!(
                            "Upgrade {n} {}? Each daemon with agents or orchestration roles running asks before it stops any.",
                            if n == 1 { "daemon" } else { "daemons" }
                        ),
                        bold(),
                    ));
                }
            }
            Phase::Running(run) => {
                lines.push((
                    if run.stopping {
                        "Stopping after this daemon…".to_string()
                    } else {
                        "Upgrading, one daemon at a time…".to_string()
                    },
                    bold(),
                ));
                for (index, planned) in run.plan.decks.iter().enumerate() {
                    let (text, style) = match (&run.finished[index], &planned.check) {
                        (Some(report), _) => (report.line.clone(), status_style(report.status)),
                        (None, DeckCheck::Upgrade { .. }) if run.current == Some(index) => (
                            format!(
                                "Upgrading: {}…",
                                stage_label(run.stage.unwrap_or(UpgradeStage::Installing))
                            ),
                            selected_style(),
                        ),
                        (None, DeckCheck::Upgrade { .. }) => {
                            ("Waiting to be upgraded.".to_string(), plain())
                        }
                        (None, check) => (planned.line(), check_style(check)),
                    };
                    lines.push((format!("{}: {text}", deck(&planned.deck)), style));
                }
            }
            Phase::Done(report) => {
                lines.push((report.headline(), bold()));
                for result in &report.decks {
                    lines.push((
                        format!("{}: {}", deck(&result.deck), result.line),
                        status_style(result.status),
                    ));
                }
            }
        }
        lines
    }

    /// The open question, with what it would stop; empty when no daemon is
    /// asking.
    pub fn question_lines(&self) -> Vec<(String, Style)> {
        let Phase::Running(Run {
            question: Some(question),
            ..
        }) = &self.phase
        else {
            return Vec::new();
        };
        let deck = display_line(&question.deck, crate::untrusted_text::REMOTE_NAME_MAX_BYTES);
        let mut lines = Vec::new();
        if question.stale {
            lines.push((
                "What is running changed since you were asked, so here is the list again."
                    .to_string(),
                plain(),
            ));
        }
        lines.push((
            format!(
                "Restarting the daemon on {deck} stops {}:",
                stop_set_count(&question.at_stake)
            ),
            bold(),
        ));
        for line in crate::daemon_upgrade_batch::stop_set_lines(&question.at_stake) {
            lines.push((format!("  {line}"), plain()));
        }
        lines.push((
            "Keep current daemon leaves them running on the old version; the new version stays installed for its next restart."
                .to_string(),
            plain(),
        ));
        lines
    }
}

/// The plan's headline, the first line of [`BatchPlan::summary`].
fn plan_headline(plan: &BatchPlan) -> String {
    plan.summary()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// What each stage is called while it runs, the desktop's words.
pub fn stage_label(stage: UpgradeStage) -> &'static str {
    match stage {
        UpgradeStage::Installing => "installing the new version",
        UpgradeStage::Restarting => "restarting the daemon",
        UpgradeStage::Verifying => "checking the new daemon answers",
    }
}

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

fn failure_style() -> Style {
    Style::default().fg(Color::Red)
}

fn status_style(status: DeckStatus) -> Style {
    match status {
        DeckStatus::Upgraded => Style::default().fg(Color::Green),
        DeckStatus::Kept | DeckStatus::Skipped => plain(),
        DeckStatus::Failed => failure_style(),
    }
}

fn check_style(check: &DeckCheck) -> Style {
    match check {
        DeckCheck::Upgrade { .. } => plain(),
        DeckCheck::Skip { .. } => plain().add_modifier(Modifier::DIM),
        DeckCheck::Unreachable { .. } => failure_style(),
    }
}

/// The hint on the dialog's last row.
fn hint(dialog: &UpgradeAllDialog) -> &'static str {
    match dialog.phase() {
        Phase::Checking => "Esc closes",
        Phase::Running(run) if run.question.is_some() && !dialog.question_seen => {
            "PgDn to read the whole list before Restart now · Esc keeps the current daemon"
        }
        Phase::Running(run) if run.question.is_some() => {
            "↑/↓ choose · Enter confirms · PgUp/PgDn scroll · Esc keeps the current daemon"
        }
        Phase::Running(run) if run.stopping => "Wait for the daemon being upgraded to finish.",
        Phase::Running(_) => "Enter or Esc stops after this daemon",
        Phase::Plan(_) => "↑/↓ choose · Enter confirms · PgUp/PgDn scroll · Esc closes",
        Phase::Failed { .. } | Phase::Done(_) => "PgUp/PgDn scroll · Enter or Esc closes",
    }
}

/// Draw `dialog` centred over `area`. Returns each button's row, for the
/// mouse.
pub fn render(frame: &mut Frame, area: Rect, dialog: &mut UpgradeAllDialog) -> Vec<(Choice, Rect)> {
    let width = area.width.saturating_sub(4).min(100);
    let max_height = area.height.saturating_sub(2);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let block = Block::default()
        .title(format!(" {TITLE} "))
        .title_style(selected_style())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let text_width = usize::from(width.saturating_sub(4)).max(1);

    let push = |body: &mut Vec<Line<'static>>, lines: Vec<(String, Style)>| {
        for (text, style) in lines {
            let clean = crate::untrusted_text::strip_control_and_bidi(&text, false);
            for row in crate::upgrade_dialog::wrap(&clean, text_width) {
                body.push(Line::styled(row, style));
            }
        }
    };
    let mut body: Vec<Line<'static>> = Vec::new();
    push(&mut body, dialog.body_lines());
    // A daemon's question follows the deck list, in the part that scrolls, so
    // a long list of agents can be read in full on any terminal.
    let question = dialog.question_lines();
    let question_start = (!question.is_empty()).then(|| {
        body.push(Line::from(""));
        body.len()
    });
    push(&mut body, question);

    let buttons = dialog.buttons();
    let selected = dialog.selected();
    let mut footer: Vec<Line<'static>> = vec![Line::from("")];
    let first_button_row = footer.len();
    for choice in &buttons {
        let (cursor, style) = if Some(*choice) == selected {
            (">", selected_style())
        } else {
            (" ", plain())
        };
        footer.push(Line::styled(format!("{cursor} {}", choice.label()), style));
    }
    footer.push(Line::from(""));

    // Border (2), the blank row above the body (1) and the hint (1) leave
    // the rest.
    let room = usize::from(max_height).saturating_sub(2 + 1 + footer.len() + 1);
    let max_scroll = body.len().saturating_sub(room);
    if let Some(start) = question_start
        && std::mem::take(&mut dialog.scroll_to_question)
    {
        dialog.scroll = start;
    }
    dialog.scroll = dialog.scroll.min(max_scroll);
    let scroll = dialog.scroll;
    // The question's last row reached the screen: Restart now may be chosen.
    if question_start.is_some() && room > 0 && scroll + room >= body.len() {
        dialog.question_seen = true;
    }
    // Chosen once this frame has said whether the question was seen.
    footer.push(Line::styled(
        hint(dialog),
        plain().add_modifier(Modifier::DIM),
    ));
    if scroll + room < body.len() {
        footer[0] = Line::styled("↓ more — PgDn", Style::default().fg(Color::Yellow));
    }
    let visible: Vec<Line<'static>> = body.into_iter().skip(scroll).take(room).collect();
    let mut text = vec![if scroll > 0 {
        Line::styled("↑ more — PgUp", Style::default().fg(Color::Yellow))
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
    let inner = block.inner(popup);
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(text), padded);

    let top = 1 + body_rows + first_button_row;
    buttons
        .iter()
        .enumerate()
        .filter_map(|(n, choice)| {
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

// ---------------------------------------------------------------------------
// The background work
// ---------------------------------------------------------------------------

/// The check and the run behind one dialog. Its updates are drained once a
/// frame ([`Job::poll`]).
pub struct Job {
    updates: Receiver<Update>,
    sender: Sender<Update>,
    decks: Arc<Vec<RemoteBatchDeck>>,
    plan: Option<BatchPlan>,
    stop: Arc<AtomicBool>,
    /// Where the answer to the open question goes.
    reply: Option<Sender<RestartChoice>>,
    /// The check or the run in progress, and whether it is the run: one that
    /// ends without its final update is reported rather than left spinning.
    worker: Option<(tokio::task::JoinHandle<()>, bool)>,
}

impl Drop for Job {
    /// The TUI closing, or the dialog going: no deck after the current one
    /// starts, and a daemon waiting on an answer keeps its current daemon.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.answer(RestartChoice::KeepCurrent);
    }
}

impl Job {
    /// Read the deck list and check every deck on `handle`'s blocking pool.
    pub fn check(handle: &tokio::runtime::Handle) -> Self {
        let (sender, updates) = channel();
        let path = crate::remote::default_remotes_path();
        let mut job = Self {
            updates,
            sender: sender.clone(),
            decks: Arc::new(Vec::new()),
            plan: None,
            stop: Arc::new(AtomicBool::new(false)),
            reply: None,
            worker: None,
        };
        match RemoteBatchDeck::from_deck_list(&path, no_report) {
            Ok(decks) => {
                let decks = Arc::new(decks);
                job.decks = Arc::clone(&decks);
                let worker = handle.spawn_blocking(move || {
                    let refs: Vec<&dyn BatchDeck> =
                        decks.iter().map(|d| d as &dyn BatchDeck).collect();
                    let _ = sender.send(Update::Checked(Ok(check_all(&refs))));
                });
                job.worker = Some((worker, false));
            }
            Err(reason) => {
                let _ = sender.send(Update::Checked(Err(reason)));
            }
        }
        job
    }

    /// Upgrade the decks the check picked.
    pub fn run(&mut self, handle: &tokio::runtime::Handle) {
        let Some(plan) = self.plan.clone() else {
            return;
        };
        let decks = Arc::clone(&self.decks);
        let stop = Arc::clone(&self.stop);
        let sender = self.sender.clone();
        let worker = handle.spawn_blocking(move || {
            let refs: Vec<&dyn BatchDeck> = decks.iter().map(|d| d as &dyn BatchDeck).collect();
            let decider = ChannelDecider {
                sender: sender.clone(),
            };
            let report = run_plan(
                &refs,
                &plan,
                &decider,
                &|| stop.load(Ordering::SeqCst),
                &mut |event| {
                    let _ = sender.send(Update::Event(event));
                },
            );
            let _ = sender.send(Update::Finished(report));
        });
        self.worker = Some((worker, true));
    }

    /// Start no deck after the current one.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Answer the open question. With none open, nothing happens.
    pub fn answer(&mut self, choice: RestartChoice) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(choice);
        }
    }

    /// Hand every update that arrived to `dialog`. A run whose work ended
    /// without a report is reported as stopped rather than left running.
    pub fn poll(&mut self, dialog: &mut UpgradeAllDialog) {
        // Read before draining: a worker's last update is sent before it
        // ends, so once it has ended everything it sent is in the channel.
        let ended = self
            .worker
            .as_ref()
            .is_some_and(|(worker, _)| worker.is_finished());
        loop {
            match self.updates.try_recv() {
                Ok(update) => {
                    if let Update::Checked(Ok(plan)) = &update {
                        self.plan = Some(plan.clone());
                    }
                    if let Update::Asked(_, reply) = &update {
                        self.reply = Some(reply.clone());
                    }
                    dialog.apply(&update);
                }
                Err(TryRecvError::Empty) => break,
                // The job holds its own sender, so this is never reached.
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if ended && let Some((_, started)) = self.worker.take() {
            let waiting = matches!(dialog.phase(), Phase::Checking | Phase::Running(_));
            if waiting {
                dialog.apply(&Update::Stopped {
                    reason: "the work behind the dialog ended without reporting".to_string(),
                    started,
                });
            }
        }
    }
}

/// Asks the dialog, and waits for its answer. No answer, because the dialog
/// went away, keeps the current daemon.
struct ChannelDecider {
    sender: Sender<Update>,
}

impl RestartDecider for ChannelDecider {
    fn decide(&self, deck: &str, at_stake: &RestartStopSet, stale: bool) -> RestartChoice {
        let (reply, answer) = channel();
        let question = Question {
            deck: deck.to_string(),
            at_stake: at_stake.clone(),
            stale,
        };
        if self.sender.send(Update::Asked(question, reply)).is_err() {
            return RestartChoice::KeepCurrent;
        }
        answer.recv().unwrap_or(RestartChoice::KeepCurrent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::RestartAgent;
    use crate::daemon_upgrade::UpgradeOutcome;
    use crate::daemon_upgrade_batch::{DeckResult, PlannedDeck, SkipReason};
    use crate::state::OrchestrationRoleRecord;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn plan() -> BatchPlan {
        BatchPlan {
            decks: vec![
                PlannedDeck {
                    deck: "alpha".into(),
                    check: DeckCheck::Upgrade {
                        from: "0.51.0".into(),
                        to: "0.52.0".into(),
                    },
                },
                PlannedDeck {
                    deck: "beta".into(),
                    check: DeckCheck::Skip {
                        reason: SkipReason::Current {
                            version: "0.52.0".into(),
                        },
                    },
                },
            ],
        }
    }

    fn question() -> Question {
        Question {
            deck: "alpha".into(),
            at_stake: RestartStopSet {
                agents: vec![RestartAgent {
                    id: "a1".into(),
                    label: "coder".into(),
                    pane_id: Some("3".into()),
                    cwd: None,
                }],
                roles: vec![OrchestrationRoleRecord {
                    pane_id: "3".into(),
                    role: "coder".into(),
                    orchestration: "team".into(),
                    is_orchestrator: false,
                }],
            },
            stale: false,
        }
    }

    #[test]
    fn enter_alone_on_the_plan_upgrades_nothing() {
        let mut dialog = UpgradeAllDialog::new();
        dialog.apply(&Update::Checked(Ok(plan())));
        assert_eq!(dialog.buttons(), [Choice::Cancel, Choice::Upgrade]);
        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
    }

    #[test]
    fn upgrade_starts_the_run_and_esc_while_running_stops_after_this_daemon() {
        let mut dialog = UpgradeAllDialog::new();
        dialog.apply(&Update::Checked(Ok(plan())));
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Start);
        assert!(dialog.is_running());
        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Effect::Stop);
        assert!(
            dialog.buttons().is_empty(),
            "stopping leaves nothing to press"
        );
        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Effect::None);
        assert_eq!(
            dialog.choose(Choice::Close),
            Effect::None,
            "a running batch cannot be closed"
        );
    }

    #[test]
    fn a_question_starts_on_keep_and_esc_keeps() {
        let mut dialog = UpgradeAllDialog::new();
        dialog.apply(&Update::Checked(Ok(plan())));
        dialog.choose(Choice::Upgrade);
        dialog.ask(question());
        assert_eq!(dialog.selected(), Some(Choice::Keep));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc)),
            Effect::Answer(RestartChoice::KeepCurrent)
        );
        dialog.ask(question());
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Effect::None,
            "Restart now waits until the question has been on screen"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(frame, area, &mut dialog);
            })
            .unwrap();
        assert!(dialog.question_seen());
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Effect::Answer(RestartChoice::RestartNow)
        );
    }

    #[test]
    fn a_plan_with_nothing_to_upgrade_offers_only_close() {
        let mut dialog = UpgradeAllDialog::new();
        let mut nothing = plan();
        nothing.decks.remove(0);
        dialog.apply(&Update::Checked(Ok(nothing)));
        assert_eq!(dialog.buttons(), [Choice::Close]);
        let body: Vec<_> = dialog.body_lines().into_iter().map(|(t, _)| t).collect();
        assert!(
            body.contains(&"Nothing to upgrade.".to_string()),
            "{body:?}"
        );
    }

    #[test]
    fn the_run_shows_each_deck_as_it_goes_then_the_results() {
        let mut dialog = UpgradeAllDialog::new();
        dialog.apply(&Update::Checked(Ok(plan())));
        dialog.choose(Choice::Upgrade);
        dialog.apply(&Update::Event(BatchEvent::Started {
            index: 0,
            deck: "alpha".into(),
        }));
        dialog.apply(&Update::Event(BatchEvent::Progress {
            index: 0,
            deck: "alpha".into(),
            progress: crate::daemon_upgrade::UpgradeProgress {
                stage: UpgradeStage::Restarting,
                detail: None,
            },
        }));
        let body: Vec<_> = dialog.body_lines().into_iter().map(|(t, _)| t).collect();
        assert!(
            body.contains(&"alpha: Upgrading: restarting the daemon…".to_string()),
            "{body:?}"
        );
        assert!(
            body.contains(&"beta: Skipped: already runs 0.52.0.".to_string()),
            "{body:?}"
        );
        let report = BatchReport {
            decks: vec![
                DeckReport::new(
                    "alpha",
                    DeckResult::Ran {
                        outcome: UpgradeOutcome::Restarted {
                            from_version: "0.51.0".into(),
                            to_version: "0.52.0".into(),
                            stopped: RestartStopSet::default(),
                        },
                    },
                ),
                DeckReport::new(
                    "beta",
                    DeckResult::Skipped {
                        reason: SkipReason::Current {
                            version: "0.52.0".into(),
                        },
                    },
                ),
            ],
        };
        dialog.apply(&Update::Finished(report));
        let body: Vec<_> = dialog.body_lines().into_iter().map(|(t, _)| t).collect();
        assert_eq!(
            body,
            [
                "Upgrade all: 1 upgraded, 1 skipped of 2 remote decks.",
                "alpha: Upgraded: now runs 0.52.0 (was 0.51.0).",
                "beta: Skipped: already runs 0.52.0.",
            ]
        );
        assert_eq!(dialog.buttons(), [Choice::Close]);
    }

    fn idle_job(runtime: &tokio::runtime::Runtime, started: bool, work: fn()) -> Job {
        let (sender, updates) = channel();
        Job {
            updates,
            sender,
            decks: Arc::new(Vec::new()),
            plan: None,
            stop: Arc::new(AtomicBool::new(false)),
            reply: None,
            worker: Some((runtime.spawn_blocking(work), started)),
        }
    }

    #[test]
    fn a_run_that_ends_without_its_report_is_reported_not_left_running() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut dialog = UpgradeAllDialog::new();
        dialog.apply(&Update::Checked(Ok(plan())));
        dialog.choose(Choice::Upgrade);
        let mut job = idle_job(&runtime, true, || panic!("the run's worker panicked"));
        while !job.worker.as_ref().unwrap().0.is_finished() {
            std::thread::yield_now();
        }
        job.poll(&mut dialog);
        assert!(
            matches!(dialog.phase(), Phase::Failed { started: true, .. }),
            "{:?}",
            dialog.phase()
        );
        assert_eq!(dialog.buttons(), [Choice::Close], "it can be closed");
    }

    #[test]
    fn dropping_the_job_stops_the_batch_and_keeps_a_waiting_daemon() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut job = idle_job(&runtime, true, || {});
        let (reply, answer) = channel();
        job.reply = Some(reply);
        let stop = Arc::clone(&job.stop);
        drop(job);
        assert!(stop.load(Ordering::SeqCst));
        assert_eq!(answer.recv().unwrap(), RestartChoice::KeepCurrent);
    }

    #[test]
    fn the_decider_waits_for_the_dialogs_answer_and_a_gone_dialog_keeps() {
        let (sender, updates) = channel();
        let decider = ChannelDecider { sender };
        let asking =
            std::thread::spawn(move || decider.decide("alpha", &RestartStopSet::default(), false));
        match updates.recv().unwrap() {
            Update::Asked(question, reply) => {
                assert_eq!(question.deck, "alpha");
                reply.send(RestartChoice::RestartNow).unwrap();
            }
            _ => panic!("a question was expected"),
        }
        assert_eq!(asking.join().unwrap(), RestartChoice::RestartNow);

        let (sender, updates) = channel();
        let decider = ChannelDecider { sender };
        let asking =
            std::thread::spawn(move || decider.decide("alpha", &RestartStopSet::default(), false));
        // The dialog takes the question and goes away without answering.
        drop(updates.recv().unwrap());
        assert_eq!(asking.join().unwrap(), RestartChoice::KeepCurrent);
    }
}
