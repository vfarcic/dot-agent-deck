//! PRD #1885: the TUI's trust prompt, `Review project commands`.
//!
//! Before the TUI opens an orchestration from a project's `.dot-agent-deck.toml`
//! — from the New Agent form (`Ctrl+n`) or when it restores a saved session — it
//! asks the attached daemon whether that orchestration's commands and prompts
//! are approved as they now read ([`crate::pane::PaneController::project_trust_state`]).
//! An approved, unchanged orchestration opens with no prompt. Otherwise this
//! prompt shows the project, the orchestration and every role's name, command,
//! description and prompt text, marking a role whose text changed since the
//! approval; `a` approves exactly the text shown (its digest goes back to the
//! daemon, which refuses if the file changed meanwhile) and then opens it, and
//! `Esc` opens and stores nothing.
//!
//! This module holds what is independent of `ui.rs`'s state: the decision
//! ([`decide`]), the words, and the widget ([`render`], with
//! [`render_to_buffer`] as the L1 test entry point). Every piece of config text
//! is drawn through [`escape_control_and_bidi`], so a control character or a
//! bidi override in it is shown, never obeyed.

use std::path::Path;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::daemon_client::GatedQuery;
use crate::project_config::OrchestrationConfig;
use crate::project_trust::{ApprovedRole, ProjectTrustView, RoleChange, TrustCheck};
use crate::untrusted_text::escape_control_and_bidi;

/// The prompt's title.
pub const TITLE: &str = "Review project commands";

/// What the prompt says of an orchestration nothing was ever approved for.
pub const NOT_APPROVED_BEFORE: &str = "not approved before";

/// What it says of one whose approved text changed.
pub const CHANGED_SINCE_APPROVAL: &str = "changed since you approved it";

/// The label of a role's `start` flag. The prompt shows the flag on every role
/// in both clients (`desktop/src/lib/projectTrust.ts` carries the same words),
/// so moving the orchestration's start role, or nothing but a flag, is visible.
pub const START_LABEL: &str = "start";

/// The value shown for `start = true`: the orchestration starts with this role.
pub const START_TRUE: &str = "true — the orchestration starts with this role";

/// The value shown for `start = false`.
pub const START_FALSE: &str = "false";

/// The label of a role's `clear` flag.
pub const CLEAR_LABEL: &str = "clear";

/// The value shown for `clear = true`: the role's agent is restarted before
/// each task it is given.
pub const CLEAR_TRUE: &str = "true — restarted before each task";

/// The value shown for `clear = false`: the role's agent keeps running between
/// tasks.
pub const CLEAR_FALSE: &str = "false — keeps running between tasks";

/// The status line when the daemon approved the orchestration but the TUI's
/// own read of the config no longer matches what was approved: the file moved
/// again while it was being opened, so nothing was started.
pub const CHANGED_WHILE_OPENING: &str = "Orchestration not started: its config changed while it \
     was being opened. Choose it again to review the new text.";

/// Prefixed to the refusal when the daemon is too old to answer trust
/// requests: the TUI checked the approval store itself, and does not prompt.
pub const OLDER_DAEMON_NOTE: &str =
    "The deck daemon is older than this TUI and cannot show the review prompt.";

/// What approving does: open the chosen orchestration, or restore a saved one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    /// From the New Agent form.
    Start,
    /// From session restore.
    Restore,
}

impl PromptAction {
    /// The approve key and what it does, as the footer shows it.
    pub fn approve_label(self) -> &'static str {
        match self {
            PromptAction::Start => "a approve and start",
            PromptAction::Restore => "a approve and restore",
        }
    }

    /// The footer: `a approve and start · Esc cancel · ↑↓ scroll`.
    pub fn footer(self) -> String {
        format!("{} · Esc cancel · ↑↓ scroll", self.approve_label())
    }
}

/// What to do with an orchestration the user chose (or a session saved).
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// Open it: the daemon says it is approved, and this is exactly the
    /// approved text.
    Open(OrchestrationConfig),
    /// Ask first: the daemon's view of it, never approved or changed.
    Prompt(ProjectTrustView),
    /// Open nothing, and say why.
    Refuse(String),
}

/// Whether `orchestration`'s resolved roles are exactly the text `view`
/// describes — the text the daemon approved or would approve.
pub fn roles_match(orchestration: &OrchestrationConfig, view: &ProjectTrustView) -> bool {
    orchestration.roles.len() == view.roles.len()
        && orchestration
            .roles
            .iter()
            .zip(&view.roles)
            .all(|(role, shown)| ApprovedRole::from_config(role) == shown.text)
}

/// Decide what opening `chosen` (from the config in `dir`) does, from the
/// daemon's `answer` to the trust-state request.
///
/// - An approved view whose text is `chosen`'s opens `chosen`. One whose text
///   differs from `chosen` — the TUI read the file before it last changed —
///   opens what `reread` reads now when that is the approved text, and
///   otherwise nothing.
/// - A view that is not approved prompts.
/// - A daemon that does not serve trust requests is older than this TUI and
///   gates nothing itself, so the TUI checks the approval store directly with
///   `local` (read-only: it never writes an approval) and opens `chosen` only
///   when it is approved; otherwise it refuses with the text that names the
///   approving command. It never prompts then, because it cannot approve
///   through such a daemon.
/// - A request that failed opens nothing.
pub fn decide(
    answer: Result<GatedQuery<ProjectTrustView>, String>,
    chosen: &OrchestrationConfig,
    reread: impl FnOnce() -> Option<OrchestrationConfig>,
    local: impl FnOnce() -> TrustCheck,
) -> Gate {
    match answer {
        Ok(GatedQuery::Answered(view)) if view.state == "approved" => {
            if roles_match(chosen, &view) {
                return Gate::Open(chosen.clone());
            }
            match reread() {
                Some(fresh) if fresh.name == chosen.name && roles_match(&fresh, &view) => {
                    Gate::Open(fresh)
                }
                _ => Gate::Refuse(CHANGED_WHILE_OPENING.to_string()),
            }
        }
        Ok(GatedQuery::Answered(view)) => Gate::Prompt(view),
        Ok(GatedQuery::Unsupported) => {
            let check = local();
            match check.refusal() {
                None => Gate::Open(chosen.clone()),
                Some(refusal) => {
                    Gate::Refuse(format!("{OLDER_DAEMON_NOTE} Not started: {refusal}"))
                }
            }
        }
        Err(e) => Gate::Refuse(escape_control_and_bidi(&format!(
            "Orchestration not started: its approval could not be checked ({e})."
        ))),
    }
}

/// [`decide`], with the TUI's own read-only check of the approval store for an
/// older daemon.
pub fn decide_for_dir(
    answer: Result<GatedQuery<ProjectTrustView>, String>,
    dir: &Path,
    chosen: &OrchestrationConfig,
    reread: impl FnOnce() -> Option<OrchestrationConfig>,
) -> Gate {
    decide(answer, chosen, reread, || {
        crate::project_trust::check_blocking(
            &crate::project_trust::TrustStore::default_location(),
            dir,
            chosen,
        )
    })
}

/// What re-sending a running orchestration's instructions to its start role
/// (the remit re-arm after a compaction or a `/clear`) may publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rearm {
    /// The text being recomposed is the approved text.
    Publish,
    /// The text the tab holds is not the approved text, but the config as it
    /// now reads is, with the same roles in the same order: re-arm from that.
    PublishApproved(OrchestrationConfig),
    /// Publish nothing, and say why.
    Refuse(String),
}

/// The lead of every re-arm refusal.
fn rearm_lead(name: &str) -> String {
    format!("Orchestration '{name}': its instructions were not sent to its coordinator again")
}

/// A re-arm refusal, escaped for display like [`crate::project_trust::refusal_text`]:
/// it names the config's orchestration and path and can quote the daemon's
/// error, so a control or bidi character in any of them is shown, never
/// obeyed.
fn refuse_rearm(message: String) -> Rearm {
    Rearm::Refuse(escape_control_and_bidi(&message))
}

/// Decide what a remit re-arm of `current` — the orchestration text a running
/// tab holds, which a warm reattach read from the config without asking — may
/// publish, from the daemon's `answer` to the trust-state request for it.
///
/// A re-arm is an unattended path: it never prompts. It publishes `current`
/// only when the daemon says the config is approved and its text is
/// `current`'s; when the config as `reread` reads it now is the approved text
/// with the same roles, it publishes that instead. With an older daemon it
/// checks `current` itself against the approval store with `local`. Anything
/// else refuses with the command that approves it.
pub fn decide_rearm(
    answer: Result<GatedQuery<ProjectTrustView>, String>,
    current: &OrchestrationConfig,
    reread: impl FnOnce() -> Option<OrchestrationConfig>,
    local: impl FnOnce() -> TrustCheck,
) -> Rearm {
    let lead = rearm_lead(&current.name);
    match answer {
        Ok(GatedQuery::Answered(view)) if view.state == "approved" => {
            if roles_match(current, &view) {
                return Rearm::Publish;
            }
            match reread() {
                Some(fresh) if fresh.name == current.name && roles_match(&fresh, &view) => {
                    let same_roles = fresh.roles.len() == current.roles.len()
                        && fresh
                            .roles
                            .iter()
                            .zip(&current.roles)
                            .all(|(a, b)| a.name == b.name && a.start == b.start);
                    if same_roles {
                        Rearm::PublishApproved(fresh)
                    } else {
                        refuse_rearm(format!(
                            "{lead}: its roles changed since it was opened. Open it again to \
                             use the new roles."
                        ))
                    }
                }
                _ => refuse_rearm(format!(
                    "{lead}: its config changed while it was being checked."
                )),
            }
        }
        Ok(GatedQuery::Answered(view)) => {
            let state = if view.state == "changed" {
                CHANGED_SINCE_APPROVAL
            } else {
                "was never approved"
            };
            refuse_rearm(format!(
                "{lead}, because its config {state}. Review and approve it from a terminal \
                 outside the deck with: {}",
                crate::project_trust::approve_command(
                    Path::new(&view.config_dir),
                    &view.orchestration
                )
            ))
        }
        Ok(GatedQuery::Unsupported) => match local().refusal() {
            None => Rearm::Publish,
            Some(refusal) => refuse_rearm(format!("{lead}. {OLDER_DAEMON_NOTE} {refusal}")),
        },
        Err(e) => refuse_rearm(format!("{lead}: its approval could not be checked ({e}).")),
    }
}

/// [`decide_rearm`], with the TUI's own read-only check of `current` against
/// the approval store for an older daemon.
pub fn decide_rearm_for_dir(
    answer: Result<GatedQuery<ProjectTrustView>, String>,
    dir: &Path,
    current: &OrchestrationConfig,
    reread: impl FnOnce() -> Option<OrchestrationConfig>,
) -> Rearm {
    decide_rearm(answer, current, reread, || {
        crate::project_trust::check_blocking(
            &crate::project_trust::TrustStore::default_location(),
            dir,
            current,
        )
    })
}

/// Whether an approval request failed because the config changed after the
/// prompt showed it ([`crate::daemon_protocol::PROJECT_ERR_TRUST_TEXT_CHANGED`]):
/// the prompt then shows the new text and asks again.
pub fn is_text_changed(error: &str) -> bool {
    error.contains(crate::daemon_protocol::PROJECT_ERR_TRUST_TEXT_CHANGED)
}

fn label_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn value_style() -> Style {
    Style::default().fg(Color::White)
}

fn mark_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// `label` then `value` on rows of `width` columns, continuation rows indented
/// under the value.
fn push_field(out: &mut Vec<Line<'static>>, indent: usize, label: &str, value: &str, width: usize) {
    let lead = format!("{}{label}: ", " ".repeat(indent));
    let room = width.saturating_sub(lead.chars().count()).max(8);
    let rows = crate::upgrade_dialog::wrap(&escape_control_and_bidi(value), room);
    for (n, row) in rows.into_iter().enumerate() {
        if n == 0 {
            out.push(Line::from(vec![
                Span::styled(lead.clone(), label_style()),
                Span::styled(row, value_style()),
            ]));
        } else {
            out.push(Line::styled(
                format!("{}{row}", " ".repeat(lead.chars().count())),
                value_style(),
            ));
        }
    }
}

/// A possibly multi-line text field: on one row after its label when it is
/// one line, otherwise under it, every line escaped and wrapped.
fn push_text(
    out: &mut Vec<Line<'static>>,
    indent: usize,
    label: &str,
    text: Option<&str>,
    width: usize,
) {
    match text {
        None => push_field(out, indent, label, "(none)", width),
        Some(text) if !text.contains('\n') => push_field(out, indent, label, text, width),
        Some(text) => {
            out.push(Line::styled(
                format!("{}{label}:", " ".repeat(indent)),
                label_style(),
            ));
            let pad = " ".repeat(indent + 2);
            let room = width.saturating_sub(indent + 2).max(8);
            for line in text.lines() {
                for row in crate::upgrade_dialog::wrap(&escape_control_and_bidi(line), room) {
                    out.push(Line::styled(format!("{pad}{row}"), value_style()));
                }
            }
        }
    }
}

/// The prompt's body for `view`, wrapped to `width` columns: the project, the
/// orchestration and its state, then every role's name (marked `changed` or
/// `new`), command, agent, `start` and `clear` flags, description and prompt
/// text, then any role the approval had that the orchestration no longer has.
pub fn body_lines(view: &ProjectTrustView, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    push_field(&mut out, 0, "Project", &view.project, width);
    if view.config_dir != view.project {
        push_field(&mut out, 0, "Config in", &view.config_dir, width);
    }
    push_field(&mut out, 0, "Orchestration", &view.orchestration, width);
    let state = match view.state.as_str() {
        "never_approved" => NOT_APPROVED_BEFORE,
        "changed" => CHANGED_SINCE_APPROVAL,
        _ => "approved",
    };
    out.push(Line::from(vec![
        Span::styled("State: ", label_style()),
        Span::styled(state.to_string(), mark_style()),
    ]));
    out.push(Line::from(""));
    for row in crate::upgrade_dialog::wrap(
        "Starting it runs these commands and gives these prompts to its agents. Approve only \
         what you trust.",
        width.max(8),
    ) {
        out.push(Line::styled(row, value_style()));
    }
    for role in &view.roles {
        out.push(Line::from(""));
        let mut head = vec![
            Span::styled("Role: ", label_style()),
            Span::styled(
                escape_control_and_bidi(&role.text.name),
                value_style().add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(change) = role.change {
            head.push(Span::styled(
                format!(
                    "  {}",
                    match change {
                        RoleChange::Changed => "changed",
                        RoleChange::New => "new",
                    }
                ),
                mark_style(),
            ));
        }
        out.push(Line::from(head));
        push_field(&mut out, 2, "command", &role.text.command, width);
        if let Some(agent) = &role.text.agent {
            push_field(&mut out, 2, "agent", agent, width);
        }
        // Both flags on every role, so moving the start role or changing
        // nothing but a flag is visible in what is approved.
        push_field(
            &mut out,
            2,
            START_LABEL,
            if role.text.start {
                START_TRUE
            } else {
                START_FALSE
            },
            width,
        );
        push_field(
            &mut out,
            2,
            CLEAR_LABEL,
            if role.text.clear {
                CLEAR_TRUE
            } else {
                CLEAR_FALSE
            },
            width,
        );
        push_text(
            &mut out,
            2,
            "description",
            role.text.description.as_deref(),
            width,
        );
        push_text(
            &mut out,
            2,
            "prompt",
            role.text.prompt_template.as_deref(),
            width,
        );
    }
    if !view.removed_roles.is_empty() {
        out.push(Line::from(""));
        // `push_field` escapes the joined names.
        push_field(
            &mut out,
            0,
            "Removed since you approved it",
            &view.removed_roles.join(", "),
            width,
        );
    }
    out
}

/// Where [`render`] put the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptLayout {
    /// The popup inside its border.
    pub inner: Rect,
    /// The footer row's position, `(x, y)`, when the popup has room for it.
    pub footer: Option<(u16, u16)>,
    /// The furthest the body can scroll: rows of body below the last row shown
    /// at scroll 0. A `usize`, not ratatui's `u16` scroll offset, so a body
    /// longer than 65,535 rows is reachable to its end.
    pub max_scroll: usize,
}

/// The widest the popup grows.
const MAX_WIDTH: u16 = 120;

/// Draw the prompt for `view` over `area`, its body scrolled down by `scroll`
/// rows (clamped to the body's end), and say where it went. The body scrolls;
/// the footer stays on the popup's last inner row.
///
/// Only the visible window of rows is handed to ratatui, whose own scroll
/// offset is a `u16`: a body of more than 65,535 rows (a long prompt is valid
/// config) must still let the user reach every later role before approving it.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    view: &ProjectTrustView,
    action: PromptAction,
    scroll: usize,
) -> PromptLayout {
    let popup_width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
    let text_width = usize::from(popup_width.saturating_sub(4));
    let lines = body_lines(view, text_width);
    // The body, a blank row and the footer, plus the border.
    let wanted = lines.len().saturating_add(2 + 2);
    let popup_height = u16::try_from(wanted)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2))
        .max(1);
    let x = area.x + area.width.saturating_sub(popup_width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;
    let popup = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {TITLE} "))
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let footer_rows: u16 = 2;
    let body_rows = inner.height.saturating_sub(footer_rows);
    let max_scroll = lines.len().saturating_sub(usize::from(body_rows));
    let scroll = scroll.min(max_scroll);
    let body_area = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        body_rows,
    );
    // `body_lines` wraps to the width already, so one line is one row.
    let window: Vec<Line<'static>> = lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(body_rows))
        .collect();
    frame.render_widget(Paragraph::new(window), body_area);

    let footer = (inner.height >= footer_rows).then(|| {
        let row = inner.y + inner.height - 1;
        let mut spans = vec![Span::styled(
            action.footer(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        let more = match (scroll > 0, scroll < max_scroll) {
            (true, true) => "   (more above and below)",
            (true, false) => "   (more above)",
            (false, true) => "   (more below)",
            (false, false) => "",
        };
        spans.push(Span::styled(more, label_style()));
        let at = inner.x.saturating_add(1);
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(at, row, inner.width.saturating_sub(2), 1),
        );
        (at, row)
    });
    PromptLayout {
        inner,
        footer,
        max_scroll,
    }
}

/// L1 entry point: the prompt for `view` drawn alone on a `width` × `height`
/// terminal, scrolled by `scroll` rows, as the buffer a `TestBackend` holds.
pub fn render_to_buffer(
    view: &ProjectTrustView,
    action: PromptAction,
    scroll: impl Into<usize>,
    width: u16,
    height: u16,
) -> Buffer {
    let scroll = scroll.into();
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            render(frame, area, view, action, scroll);
        })
        .expect("draw the trust prompt");
    terminal.backend().buffer().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::OrchestrationRoleConfig;
    use crate::project_trust::TrustRoleView;

    fn role(name: &str, command: &str) -> OrchestrationRoleConfig {
        OrchestrationRoleConfig {
            agent: None,
            name: name.to_string(),
            command: command.to_string(),
            start: name == "orchestrator",
            description: Some(format!("{name} description")),
            prompt_template: Some(format!("{name} prompt")),
            clear: false,
        }
    }

    fn orchestration(coder_command: &str) -> OrchestrationConfig {
        OrchestrationConfig {
            default: false,
            name: "team".to_string(),
            roles: vec![role("orchestrator", "cat"), role("coder", coder_command)],
        }
    }

    fn view_of(orch: &OrchestrationConfig, state: &str) -> ProjectTrustView {
        ProjectTrustView {
            project: "/work/project".to_string(),
            config_dir: "/work/project".to_string(),
            orchestration: orch.name.clone(),
            state: state.to_string(),
            roles: orch
                .roles
                .iter()
                .map(|r| TrustRoleView {
                    text: ApprovedRole::from_config(r),
                    change: None,
                })
                .collect(),
            removed_roles: Vec::new(),
            digest: "d".to_string(),
            config_revision: None,
        }
    }

    fn unused_check() -> TrustCheck {
        panic!("the store is read only for a daemon that cannot answer")
    }

    fn grid(buffer: &Buffer) -> String {
        let area = buffer.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_approved_unchanged_orchestration_opens_as_chosen() {
        let chosen = orchestration("cat");
        let gate = decide(
            Ok(GatedQuery::Answered(view_of(&chosen, "approved"))),
            &chosen,
            || panic!("no re-read for a match"),
            unused_check,
        );
        assert_eq!(gate, Gate::Open(chosen));
    }

    #[test]
    fn an_unapproved_or_changed_orchestration_prompts() {
        let chosen = orchestration("cat");
        for state in ["never_approved", "changed"] {
            let view = view_of(&chosen, state);
            let gate = decide(
                Ok(GatedQuery::Answered(view.clone())),
                &chosen,
                || None,
                unused_check,
            );
            assert_eq!(gate, Gate::Prompt(view));
        }
    }

    /// The form read the config before it changed and the new text was then
    /// approved: what opens is the new read, never the stale copy, and nothing
    /// opens when the new read is not the approved text either.
    #[test]
    fn an_approval_of_newer_text_opens_only_the_matching_read() {
        let stale = orchestration("cat");
        let approved = orchestration("cut");
        let view = view_of(&approved, "approved");
        let gate = decide(
            Ok(GatedQuery::Answered(view.clone())),
            &stale,
            || Some(approved.clone()),
            unused_check,
        );
        assert_eq!(gate, Gate::Open(approved));
        let gate = decide(
            Ok(GatedQuery::Answered(view)),
            &stale,
            || Some(orchestration("cot")),
            unused_check,
        );
        assert_eq!(gate, Gate::Refuse(CHANGED_WHILE_OPENING.to_string()));
    }

    /// A remit re-arm publishes the text the tab holds only when it is the
    /// approved text; it never prompts, and a changed or never-approved
    /// config refuses with the approving command.
    #[test]
    fn a_rearm_publishes_only_approved_text() {
        let held = orchestration("cat");
        let rearm = decide_rearm(
            Ok(GatedQuery::Answered(view_of(&held, "approved"))),
            &held,
            || panic!("no re-read for a match"),
            unused_check,
        );
        assert_eq!(rearm, Rearm::Publish);

        for state in ["never_approved", "changed"] {
            match decide_rearm(
                Ok(GatedQuery::Answered(view_of(&held, state))),
                &held,
                || None,
                unused_check,
            ) {
                Rearm::Refuse(message) => {
                    assert!(message.contains("'team'"), "{message}");
                    assert!(
                        message.contains(
                            "dot-agent-deck trust approve /work/project --orchestration team"
                        ),
                        "{message}"
                    );
                }
                other => panic!("{state}: expected a refusal, got {other:?}"),
            }
        }

        assert!(matches!(
            decide_rearm(Err("no answer".to_string()), &held, || None, unused_check),
            Rearm::Refuse(m) if m.contains("could not be checked")
        ));
    }

    /// The tab holds text a warm reattach read before the config changed: the
    /// newer read is re-armed when it is the approved text with the same
    /// roles, and nothing when the roles moved or the read is not approved.
    #[test]
    fn a_rearm_of_stale_text_uses_only_an_approved_read_with_the_same_roles() {
        let held = orchestration("cat");
        let approved = orchestration("cut");
        let view = view_of(&approved, "approved");
        assert_eq!(
            decide_rearm(
                Ok(GatedQuery::Answered(view.clone())),
                &held,
                || Some(approved.clone()),
                unused_check,
            ),
            Rearm::PublishApproved(approved.clone())
        );
        assert!(matches!(
            decide_rearm(
                Ok(GatedQuery::Answered(view)),
                &held,
                || Some(orchestration("cot")),
                unused_check,
            ),
            Rearm::Refuse(_)
        ));

        let mut renamed = approved.clone();
        renamed.roles[1].name = "writer".to_string();
        assert!(matches!(
            decide_rearm(
                Ok(GatedQuery::Answered(view_of(&renamed, "approved"))),
                &held,
                || Some(renamed.clone()),
                unused_check,
            ),
            Rearm::Refuse(m) if m.contains("roles changed")
        ));
    }

    #[test]
    fn a_failed_request_opens_nothing() {
        let chosen = orchestration("cat");
        let gate = decide(
            Err("the deck did not answer".to_string()),
            &chosen,
            || None,
            unused_check,
        );
        assert!(matches!(gate, Gate::Refuse(m) if m.contains("could not be checked")));
    }

    /// Text a terminal would obey: ESC, BEL and RIGHT-TO-LEFT OVERRIDE.
    const HOSTILE: &str = "\u{1b}[2J\u{7}\u{202e}";

    fn assert_shown_escaped(message: &str) {
        assert!(
            !message
                .chars()
                .any(|c| c.is_control() || crate::untrusted_text::is_bidi_format_char(c)),
            "a raw control or bidi character reached the refusal: {message:?}"
        );
        assert!(
            message.contains("\\u{1b}") && message.contains("\\u{202e}"),
            "the hostile text is shown escaped, not dropped: {message:?}"
        );
    }

    /// PRD #1885 (Qodo #8 on PR #2256): a refusal built from a daemon error
    /// or from the config's orchestration name shows control and bidi
    /// characters escaped, wherever the caller puts it.
    #[test]
    fn refusals_show_hostile_names_and_errors_escaped() {
        let chosen = orchestration("cat");
        match decide(
            Err(format!("boom {HOSTILE}")),
            &chosen,
            || None,
            unused_check,
        ) {
            Gate::Refuse(message) => assert_shown_escaped(&message),
            other => panic!("expected a refusal, got {other:?}"),
        }

        let mut held = orchestration("cat");
        held.name = format!("team{HOSTILE}");
        for state in ["never_approved", "changed"] {
            match decide_rearm(
                Ok(GatedQuery::Answered(view_of(&held, state))),
                &held,
                || None,
                unused_check,
            ) {
                Rearm::Refuse(message) => assert_shown_escaped(&message),
                other => panic!("{state}: expected a refusal, got {other:?}"),
            }
        }
        match decide_rearm(
            Ok(GatedQuery::Answered(view_of(
                &orchestration("cut"),
                "approved",
            ))),
            &held,
            || None,
            unused_check,
        ) {
            Rearm::Refuse(message) => assert_shown_escaped(&message),
            other => panic!("expected a refusal, got {other:?}"),
        }
        match decide_rearm(Err(format!("boom {HOSTILE}")), &held, || None, unused_check) {
            Rearm::Refuse(message) => assert_shown_escaped(&message),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// An older daemon: the TUI reads the store itself, opens an approved
    /// orchestration, and refuses an unapproved one with the approving command
    /// rather than prompting.
    #[test]
    fn an_older_daemon_is_judged_from_the_store_without_a_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(".dot-agent-deck.toml"),
            "[[orchestrations]]\nname = \"team\"\n\n\
             [[orchestrations.roles]]\nname = \"orchestrator\"\ncommand = \"cat\"\nstart = true\n",
        )
        .expect("write the config");
        let chosen = crate::project_config::load_project_config(dir.path())
            .expect("read the config")
            .expect("a config")
            .orchestrations
            .remove(0);
        let store_dir = tempfile::tempdir().expect("store dir");
        let store = crate::project_trust::TrustStore::at(store_dir.path());
        let check = || crate::project_trust::check_blocking(&store, dir.path(), &chosen);
        match decide(Ok(GatedQuery::Unsupported), &chosen, || None, check) {
            Gate::Refuse(message) => {
                assert!(message.starts_with(OLDER_DAEMON_NOTE), "{message}");
                assert!(
                    message.contains("dot-agent-deck trust approve"),
                    "{message}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let digest = check().digest();
        assert_eq!(
            crate::project_trust::approve_if_unchanged(&store, dir.path(), "team", &digest)
                .expect("approve"),
            crate::project_trust::ApproveOutcome::Approved
        );
        let gate = decide(Ok(GatedQuery::Unsupported), &chosen, || None, check);
        assert_eq!(gate, Gate::Open(chosen.clone()));
    }

    #[test]
    fn the_prompt_shows_every_role_its_marks_and_the_footer() {
        let chosen = orchestration("env X=1 cat");
        let mut view = view_of(&chosen, "changed");
        view.roles[1].change = Some(RoleChange::Changed);
        view.removed_roles = vec!["reviewer".to_string()];
        let shown = grid(&render_to_buffer(
            &view,
            PromptAction::Start,
            0usize,
            120,
            40,
        ));
        for needle in [
            TITLE,
            "/work/project",
            "team",
            CHANGED_SINCE_APPROVAL,
            "orchestrator",
            "env X=1 cat",
            "coder description",
            "coder prompt",
            "reviewer",
            "a approve and start · Esc cancel · ↑↓ scroll",
        ] {
            assert!(shown.contains(needle), "{needle:?} missing:\n{shown}");
        }
        assert!(
            shown
                .lines()
                .any(|l| l.contains("coder") && l.contains("changed")),
            "{shown}"
        );
        let never = view_of(&chosen, "never_approved");
        let shown = grid(&render_to_buffer(
            &never,
            PromptAction::Restore,
            0usize,
            120,
            40,
        ));
        assert!(shown.contains(NOT_APPROVED_BEFORE), "{shown}");
        assert!(shown.contains("a approve and restore"), "{shown}");
    }

    /// Config text is shown, never obeyed: an escape sequence or a bidi
    /// override in a command reaches the screen as visible escapes.
    #[test]
    fn control_characters_in_config_text_are_shown_escaped() {
        let chosen = orchestration("cat\u{1b}[2J\u{202e}evil");
        let view = view_of(&chosen, "never_approved");
        let shown = grid(&render_to_buffer(
            &view,
            PromptAction::Start,
            0usize,
            120,
            40,
        ));
        assert!(shown.contains("\\u{1b}[2J"), "{shown}");
        assert!(shown.contains("\\u{202e}evil"), "{shown}");
        assert!(!shown.contains('\u{1b}'));
    }

    /// A long prompt scrolls: the footer stays put, the body moves, and the
    /// scroll is clamped to the body's end.
    #[test]
    fn a_long_prompt_scrolls_under_a_fixed_footer() {
        let mut chosen = orchestration("cat");
        chosen.roles[1].prompt_template = Some(
            (1..=60)
                .map(|n| format!("LINE-{n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let view = view_of(&chosen, "never_approved");
        let top = grid(&render_to_buffer(
            &view,
            PromptAction::Start,
            0usize,
            100,
            30,
        ));
        assert!(
            top.contains("LINE-1\u{20}") || top.contains("LINE-1 "),
            "{top}"
        );
        assert!(!top.contains("LINE-60"), "{top}");
        assert!(top.contains("(more below)"), "{top}");
        let bottom = grid(&render_to_buffer(
            &view,
            PromptAction::Start,
            u16::MAX,
            100,
            30,
        ));
        assert!(bottom.contains("LINE-60"), "{bottom}");
        assert!(bottom.contains("(more above)"), "{bottom}");
        assert!(bottom.contains("a approve and start"), "{bottom}");
    }
}
