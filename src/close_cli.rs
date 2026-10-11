//! PRD #1589: the `dot-agent-deck close` command — who is asking, what it
//! names, and how the daemon's report is printed.
//!
//! The daemon decides everything that matters (`crate::close_agents`); this
//! module only turns the command line and the environment into a request and
//! the report into text and an exit code, which is why it lives in the library
//! where it can be tested rather than in `main.rs`.

use std::ffi::OsString;

use crate::daemon_client::{CloseAgentsError, DaemonClient};
use crate::daemon_protocol::{
    CallerClaim, CloseOutcome, CloseRefusalReason, CloseReport, CloseSelector, CloseTarget,
    UnitSelector, WorktreeVerdictKind,
};

/// Everything went as asked: every target closed, or listed.
pub const EXIT_OK: u8 = 0;
/// Something was refused or failed, or only partly closed.
pub const EXIT_REFUSED: u8 = 1;
/// The daemon is too old to know `close`; nothing was sent.
pub const EXIT_DAEMON_TOO_OLD: u8 = 2;
/// No daemon answered.
pub const EXIT_UNREACHABLE: u8 = 3;

/// The most characters of any producer-supplied value (a unit name, a
/// selector, a path, a daemon message) the output prints.
const MAX_SHOWN_CHARS: usize = 200;

/// Who the command runs as, decided from the three variables a deck pane's
/// environment carries.
#[derive(Clone, PartialEq, Eq)]
pub enum AmbientIdentity {
    /// None of the three is set: a person at a shell.
    Person,
    /// All three are set: an agent in a deck pane.
    Agent {
        pane_id: String,
        agent_id: String,
        token: String,
    },
}

impl std::fmt::Debug for AmbientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AmbientIdentity::Person => f.write_str("Person"),
            AmbientIdentity::Agent {
                pane_id, agent_id, ..
            } => f
                .debug_struct("Agent")
                .field("pane_id", pane_id)
                .field("agent_id", agent_id)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// PRD #1589 D2 (auditor S5): read the caller's identity from
/// `DOT_AGENT_DECK_PANE_ID`, `DOT_AGENT_DECK_AGENT_ID` and
/// `DOT_AGENT_DECK_PANE_CAPABILITY`.
///
/// All three absent is a person. Any present means an agent, and then all
/// three must be present, non-empty and Unicode — otherwise the command is
/// refused rather than run as a person, so an identity accidentally stripped
/// halfway fails closed instead of quietly gaining a person's reach. (Removing
/// all three on purpose remains possible; the deck's trust boundary is the OS
/// user.)
pub fn ambient_identity_from(
    pane: Option<OsString>,
    agent: Option<OsString>,
    capability: Option<OsString>,
) -> Result<AmbientIdentity, String> {
    if pane.is_none() && agent.is_none() && capability.is_none() {
        return Ok(AmbientIdentity::Person);
    }
    let read = |name: &str, value: Option<OsString>| -> Result<String, String> {
        let value = value.ok_or_else(|| format!("{name} is not set"))?;
        let value = value
            .into_string()
            .map_err(|_| format!("{name} is not valid Unicode"))?;
        let value = value.trim().to_string();
        if value.is_empty() {
            return Err(format!("{name} is empty"));
        }
        Ok(value)
    };
    let checked = (
        read(crate::agent_pty::DOT_AGENT_DECK_PANE_ID, pane),
        read(crate::agent_pty::DOT_AGENT_DECK_AGENT_ID, agent),
        read(
            crate::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY,
            capability,
        ),
    );
    match checked {
        (Ok(pane_id), Ok(agent_id), Ok(token)) => Ok(AmbientIdentity::Agent {
            pane_id,
            agent_id,
            token,
        }),
        (pane, agent, token) => {
            let problems: Vec<String> = [pane, agent, token]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            Err(format!(
                "refused: this shell carries part of a deck pane's identity ({}). An agent's \
                 pane sets all three of {}, {} and {}; run `close` with all three intact, or \
                 from a shell that carries none of them. Nothing was closed.",
                problems.join(", "),
                crate::agent_pty::DOT_AGENT_DECK_PANE_ID,
                crate::agent_pty::DOT_AGENT_DECK_AGENT_ID,
                crate::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY,
            ))
        }
    }
}

/// [`ambient_identity_from`] over this process's environment.
pub fn ambient_identity() -> Result<AmbientIdentity, String> {
    ambient_identity_from(
        std::env::var_os(crate::agent_pty::DOT_AGENT_DECK_PANE_ID),
        std::env::var_os(crate::agent_pty::DOT_AGENT_DECK_AGENT_ID),
        std::env::var_os(crate::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY),
    )
}

impl AmbientIdentity {
    /// The claim the request carries: none for a person.
    pub fn claim(&self) -> Option<CallerClaim> {
        match self {
            AmbientIdentity::Person => None,
            AmbientIdentity::Agent { pane_id, token, .. } => Some(CallerClaim {
                pane_id: pane_id.clone(),
                token: token.clone(),
            }),
        }
    }
}

/// The command's arguments, as clap parsed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloseArgs {
    pub units: Vec<String>,
    /// `--unit-id`: units by the daemon-issued id a listing or an `ambiguous`
    /// refusal printed.
    pub unit_ids: Vec<String>,
    pub pane: Option<String>,
    pub orchestration_of: Option<String>,
    pub all: bool,
    pub yes: bool,
    pub dry_run: bool,
    pub force: bool,
    pub json: bool,
}

impl CloseArgs {
    /// The selector these arguments name. clap enforces that exactly one is
    /// given; this reports it again for a caller that built the struct itself.
    pub fn selector(&self) -> Result<CloseSelector, String> {
        let given = usize::from(!self.units.is_empty())
            + usize::from(!self.unit_ids.is_empty())
            + usize::from(self.pane.is_some())
            + usize::from(self.orchestration_of.is_some())
            + usize::from(self.all);
        if given != 1 {
            return Err(
                "name what to close: unit names, --unit-id, --pane, --orchestration-of or --all"
                    .to_string(),
            );
        }
        Ok(if self.all {
            CloseSelector::AllUnits
        } else if !self.unit_ids.is_empty() {
            CloseSelector::UnitIds {
                ids: self.unit_ids.clone(),
            }
        } else if let Some(pane_id) = &self.pane {
            CloseSelector::Pane {
                pane_id: pane_id.clone(),
            }
        } else if let Some(pane_id) = &self.orchestration_of {
            CloseSelector::OrchestrationOf {
                pane_id: pane_id.clone(),
            }
        } else {
            CloseSelector::Units(UnitSelector {
                names: self.units.clone(),
            })
        })
    }
}

/// What the command prints and exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
}

/// Run `close`: build the request, send it (twice for `--all --yes`: preview,
/// then apply by the preview's unit ids), and render the answer.
pub async fn run_close(
    client: &DaemonClient,
    args: &CloseArgs,
    identity: &AmbientIdentity,
) -> CloseOutput {
    let selector = match args.selector() {
        Ok(selector) => selector,
        Err(message) => return error_output(args.json, message, EXIT_REFUSED),
    };
    let claim = identity.claim();
    let bulk = selector == CloseSelector::AllUnits;
    // `--all` is always sent as a preview, `--yes` or not: the daemon refuses
    // a bulk selector that is not a dry run, and `--all --yes` applies by the
    // ids this preview lists, in a second request (auditor S2).
    let dry_run = args.dry_run || bulk;
    let first = match client
        .close_agents(selector, claim.clone(), args.force, dry_run)
        .await
    {
        Ok(report) => report,
        Err(e) => return client_error_output(args.json, &e),
    };
    if !(bulk && args.yes && !args.dry_run) || first.refused.is_some() {
        return render(&first, None, args.json);
    }
    // `--all --yes`: apply exactly the units the preview listed, by id, so a
    // name reused in between cannot be retargeted. The daemon re-checks every
    // refusal when it applies.
    let ids: Vec<String> = first
        .targets
        .iter()
        .filter(|t| t.outcome == CloseOutcome::Listed)
        .filter_map(|t| t.unit_id.clone())
        .collect();
    if ids.is_empty() {
        return render(&first, None, args.json);
    }
    match client
        .close_agents(CloseSelector::UnitIds { ids }, claim, args.force, false)
        .await
    {
        Ok(applied) => render(&applied, Some(&first), args.json),
        Err(e) => client_error_output(args.json, &e),
    }
}

/// The output for an identity [`ambient_identity`] refused, honouring
/// `--json` like every other error (reviewer N3).
pub fn identity_error_output(json: bool, message: String) -> CloseOutput {
    error_output(json, message, EXIT_REFUSED)
}

fn error_output(json: bool, message: String, code: u8) -> CloseOutput {
    let stderr = format!("close: {}\n", shown(&message));
    let stdout = if json {
        format!(
            "{}\n",
            serde_json::json!({ "error": message, "exit_code": code })
        )
    } else {
        String::new()
    };
    CloseOutput {
        stdout,
        stderr,
        code,
    }
}

/// The text and exit code for a request that got no report.
pub fn client_error_output(json: bool, error: &CloseAgentsError) -> CloseOutput {
    match error {
        CloseAgentsError::TooOld { daemon_version } => error_output(
            json,
            format!(
                "the running daemon{} is too old for `close`; nothing was closed. Restart it \
                 onto this build with `dot-agent-deck daemon restart`, or close from the TUI or \
                 the desktop app.",
                daemon_version
                    .as_deref()
                    .map(|v| format!(" ({v})"))
                    .unwrap_or_default()
            ),
            EXIT_DAEMON_TOO_OLD,
        ),
        CloseAgentsError::Unreachable(e) => error_output(
            json,
            format!("no daemon is running ({e}); nothing was closed"),
            EXIT_UNREACHABLE,
        ),
        CloseAgentsError::Client(e) => error_output(json, format!("{e}"), EXIT_REFUSED),
    }
}

/// The exit code a report earns.
pub fn exit_code(report: &CloseReport) -> u8 {
    if report.refused.is_some() {
        return EXIT_REFUSED;
    }
    let bad = report
        .targets
        .iter()
        .any(|t| !matches!(t.outcome, CloseOutcome::Closed | CloseOutcome::Listed));
    if bad { EXIT_REFUSED } else { EXIT_OK }
}

/// A producer-supplied value, made safe to print: bounded, with control and
/// bidi characters escaped.
fn shown(value: &str) -> String {
    crate::config_validation::escape_field_for_log(value, MAX_SHOWN_CHARS)
}

fn target_label(t: &CloseTarget) -> String {
    let name = t.name.as_deref().unwrap_or(&t.selector);
    match &t.unit_id {
        Some(id) => format!("{} ({id}, {})", shown(name), t.kind),
        None => format!("{} ({})", shown(name), t.kind),
    }
}

fn forced_note(t: &CloseTarget) -> Option<String> {
    if t.forced_over.is_empty() {
        return None;
    }
    let parts: Vec<String> = t
        .forced_over
        .iter()
        .map(|reason| match reason {
            CloseRefusalReason::Busy => {
                let status = t
                    .panes
                    .iter()
                    .filter_map(|p| p.status.as_deref())
                    .find(|s| {
                        matches!(
                            *s,
                            "Thinking" | "Working" | "Compacting" | "WaitingForInput" | "Blocked"
                        )
                    })
                    .unwrap_or("busy");
                format!("was {status}")
            }
            CloseRefusalReason::NotReported => "had not reported".to_string(),
            CloseRefusalReason::StrandsOrchestration => {
                "one role of a live orchestration".to_string()
            }
            other => other.code().to_string(),
        })
        .collect();
    Some(format!("(forced: {})", parts.join(", ")))
}

fn verdict_text(kind: WorktreeVerdictKind) -> &'static str {
    match kind {
        WorktreeVerdictKind::Removed => "removed",
        WorktreeVerdictKind::KeptDirty => "kept: uncommitted changes",
        WorktreeVerdictKind::KeptCouldNotCheck => "kept: could not check for uncommitted changes",
        WorktreeVerdictKind::RemoveFailed => "kept: removing it failed",
        WorktreeVerdictKind::StillInUse => "kept: still in use",
        WorktreeVerdictKind::NotRecorded => {
            "not recorded by the daemon (run `dot-agent-deck worktree reclaim`)"
        }
        WorktreeVerdictKind::TimedOut => {
            "cleanup still running: the agents are stopped, the outcome is not known yet"
        }
        WorktreeVerdictKind::Unknown => "unknown",
    }
}

fn pane_lines(out: &mut String, t: &CloseTarget) {
    for pane in &t.panes {
        let mut line = format!(
            "    pane {} agent {}",
            shown(pane.pane_id.as_deref().unwrap_or("-")),
            pane.agent_id
        );
        if let Some(role) = &pane.role {
            line.push_str(&format!(" role {}", shown(role)));
        }
        if pane.is_orchestrator {
            line.push_str(" (orchestrator)");
        }
        if pane.exited {
            line.push_str(" exited");
        } else if let Some(status) = &pane.status {
            line.push_str(&format!(" {status}"));
        }
        match pane.stopped {
            Some(true) => line.push_str(" — stopped"),
            Some(false) => line.push_str(" — STILL RUNNING"),
            None => {}
        }
        out.push_str(&line);
        out.push('\n');
    }
}

fn unit_detail_lines(out: &mut String, t: &CloseTarget) {
    if let Some(reported) = t.reported {
        out.push_str(&format!(
            "    reported: {}\n",
            if reported { "yes" } else { "no" }
        ));
    }
    if let Some(worktree) = &t.worktree {
        out.push_str(&format!("    worktree: {}\n", shown(worktree)));
    }
    if let Some(branch) = &t.branch {
        out.push_str(&format!("    branch: {}\n", shown(branch)));
    }
    if let Some(clone) = &t.clone {
        out.push_str(&format!("    clone: {}\n", shown(clone)));
    }
    if let Some(dispatcher) = &t.dispatcher {
        out.push_str(&format!(
            "    dispatcher: pane {} agent {}\n",
            shown(&dispatcher.pane_id),
            dispatcher.agent_id
        ));
    }
    for d in &t.open_descendants {
        out.push_str(&format!(
            "    left open (dispatched by it): {} ({}) in {}\n",
            shown(&d.name),
            d.unit_id,
            shown(&d.clone)
        ));
    }
}

/// The human-readable report.
pub fn render_human(report: &CloseReport, preview: Option<&CloseReport>) -> String {
    let mut out = String::new();
    if let Some(preview) = preview {
        out.push_str(&render_human(preview, None));
        out.push('\n');
    }
    if let Some(refused) = &report.refused {
        out.push_str(&format!(
            "refused ({}): {}\nNothing was closed.\n",
            refused.reason.code(),
            shown(&refused.message)
        ));
        return out;
    }
    let listed: Vec<&CloseTarget> = report
        .targets
        .iter()
        .filter(|t| t.outcome == CloseOutcome::Listed)
        .collect();
    let closed: Vec<&CloseTarget> = report
        .targets
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                CloseOutcome::Closed | CloseOutcome::PartiallyClosed
            )
        })
        .collect();
    let refused: Vec<&CloseTarget> = report
        .targets
        .iter()
        .filter(|t| {
            !matches!(
                t.outcome,
                CloseOutcome::Closed | CloseOutcome::PartiallyClosed | CloseOutcome::Listed
            )
        })
        .collect();
    if report.truncated {
        out.push_str(&format!(
            "listed only the first {} units; more are running. Close these, then list again.\n",
            listed.len() + closed.len() + refused.len()
        ));
    }
    if report.dry_run {
        if listed.is_empty() && refused.is_empty() {
            out.push_str("nothing to close\n");
        } else if !listed.is_empty() {
            out.push_str("would close (dry run — nothing was closed):\n");
        }
        for t in &listed {
            out.push_str(&format!("  {}\n", target_label(t)));
            unit_detail_lines(&mut out, t);
            pane_lines(&mut out, t);
            if !t.would_refuse.is_empty() {
                let codes: Vec<&str> = t.would_refuse.iter().map(|r| r.code()).collect();
                out.push_str(&format!(
                    "    would be refused: {} (pass --force to close it anyway)\n",
                    codes.join(", ")
                ));
            }
        }
    }
    if !closed.is_empty() {
        out.push_str("closed:\n");
        for t in &closed {
            let mut line = format!("  {}", target_label(t));
            if t.outcome == CloseOutcome::PartiallyClosed {
                line.push_str(&format!(
                    " — PARTIALLY: still running: {}",
                    t.survivors
                        .iter()
                        .map(|s| shown(s))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(note) = forced_note(t) {
                line.push(' ');
                line.push_str(&note);
            }
            out.push_str(&line);
            out.push('\n');
            pane_lines(&mut out, t);
            for d in &t.open_descendants {
                out.push_str(&format!(
                    "    left open (dispatched by it): {} ({}) in {}\n",
                    shown(&d.name),
                    d.unit_id,
                    shown(&d.clone)
                ));
            }
        }
    }
    if !refused.is_empty() {
        out.push_str("refused:\n");
        for t in &refused {
            let reason = t
                .reason
                .map(|r| r.code().to_string())
                .unwrap_or_else(|| "failed".to_string());
            let detail = t.message.as_deref().or(t.error.as_deref()).unwrap_or("");
            out.push_str(&format!(
                "  {}: {reason} — {}\n",
                target_label(t),
                shown(detail)
            ));
            for c in &t.candidates {
                out.push_str(&format!(
                    "    candidate {} — worktree {} panes {}\n",
                    c.unit_id,
                    shown(&c.worktree),
                    c.panes
                        .iter()
                        .map(|p| shown(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    let worktrees: Vec<&CloseTarget> = report
        .targets
        .iter()
        .filter(|t| t.worktree_verdict.is_some())
        .collect();
    if !worktrees.is_empty() {
        out.push_str("worktrees:\n");
        for t in worktrees {
            let verdict = t.worktree_verdict.as_ref().expect("filtered");
            let branch = t
                .branch
                .as_deref()
                .map(|b| format!(" (branch {} kept)", shown(b)))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {}: {}{branch}\n",
                shown(&verdict.path),
                verdict_text(verdict.verdict)
            ));
        }
    }
    out
}

/// The `--json` report: the daemon's report, plus `closed`, `refused`,
/// `listed` and `worktrees` lists flattened from it for scripts that want one
/// field to read.
pub fn render_json(report: &CloseReport, preview: Option<&CloseReport>) -> serde_json::Value {
    let mut value = serde_json::to_value(report).unwrap_or(serde_json::Value::Null);
    let name_of = |t: &CloseTarget| t.name.clone().unwrap_or_else(|| t.selector.clone());
    let closed: Vec<serde_json::Value> = report
        .targets
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                CloseOutcome::Closed | CloseOutcome::PartiallyClosed
            )
        })
        .map(|t| {
            serde_json::json!({
                "name": name_of(t),
                "unit_id": t.unit_id,
                "partial": t.outcome == CloseOutcome::PartiallyClosed,
                "survivors": t.survivors,
                "forced_over": t.forced_over,
                "panes": t.panes.iter().filter_map(|p| p.pane_id.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut refused: Vec<serde_json::Value> = report
        .targets
        .iter()
        .filter(|t| {
            matches!(
                t.outcome,
                CloseOutcome::Refused | CloseOutcome::Failed | CloseOutcome::Unknown
            )
        })
        .map(|t| {
            serde_json::json!({
                "name": name_of(t),
                "unit_id": t.unit_id,
                "reason": t.reason.map(|r| r.code()).unwrap_or("failed"),
                "message": t.message.clone().or_else(|| t.error.clone()),
            })
        })
        .collect();
    if let Some(r) = &report.refused {
        refused.push(serde_json::json!({
            "name": null,
            "unit_id": null,
            "reason": r.reason.code(),
            "message": r.message,
        }));
    }
    let listed: Vec<serde_json::Value> = report
        .targets
        .iter()
        .filter(|t| t.outcome == CloseOutcome::Listed)
        .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null))
        .collect();
    let worktrees: Vec<serde_json::Value> = report
        .targets
        .iter()
        .filter_map(|t| {
            t.worktree_verdict.as_ref().map(|v| {
                serde_json::json!({
                    "name": name_of(t),
                    "unit_id": t.unit_id,
                    "path": v.path,
                    "verdict": v.verdict,
                    "branch": t.branch,
                })
            })
        })
        .collect();
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("closed".into(), serde_json::Value::Array(closed));
        map.insert("refused".into(), serde_json::Value::Array(refused));
        map.insert("listed".into(), serde_json::Value::Array(listed));
        map.insert("worktrees".into(), serde_json::Value::Array(worktrees));
        map.insert("exit_code".into(), serde_json::json!(exit_code(report)));
        if let Some(preview) = preview {
            map.insert("preview".into(), render_json(preview, None));
        }
    }
    value
}

fn render(report: &CloseReport, preview: Option<&CloseReport>, json: bool) -> CloseOutput {
    let code = exit_code(report);
    let stdout = if json {
        format!("{}\n", render_json(report, preview))
    } else {
        render_human(report, preview)
    };
    CloseOutput {
        stdout,
        stderr: String::new(),
        code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_protocol::{ClosePane, CloseRefusal, WorktreeVerdict};

    fn os(v: &str) -> Option<OsString> {
        Some(OsString::from(v))
    }

    #[test]
    fn no_identity_at_all_is_a_person() {
        assert_eq!(
            ambient_identity_from(None, None, None).unwrap(),
            AmbientIdentity::Person
        );
    }

    #[test]
    fn a_complete_identity_is_an_agent_and_its_token_is_never_printed() {
        let id = ambient_identity_from(os("p1"), os("7"), os("secret-token")).unwrap();
        assert!(matches!(&id, AmbientIdentity::Agent { pane_id, .. } if pane_id == "p1"));
        assert!(!format!("{id:?}").contains("secret-token"));
        let claim = id.claim().unwrap();
        assert!(!format!("{claim:?}").contains("secret-token"));
    }

    #[test]
    fn an_incomplete_ambient_identity_is_refused_not_a_person() {
        for (pane, agent, cap) in [
            (os("p1"), None, None),
            (None, os("7"), None),
            (None, None, os("tok")),
            (os("p1"), os("7"), None),
            (os(""), os("7"), os("tok")),
            (os("p1"), os("  "), os("tok")),
        ] {
            let err = ambient_identity_from(pane.clone(), agent.clone(), cap.clone())
                .expect_err("a partial identity must fail closed");
            assert!(err.contains("Nothing was closed"), "{err}");
            assert!(!err.contains("tok") || err.contains("CAPABILITY"), "{err}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_identity_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let err = ambient_identity_from(Some(bad), os("7"), os("tok")).unwrap_err();
        assert!(err.contains("not valid Unicode"), "{err}");
    }

    fn target(outcome: CloseOutcome) -> CloseTarget {
        let mut t = CloseTarget::refused("issue-1", CloseRefusalReason::Unknown, String::new());
        t.reason = None;
        t.message = None;
        t.outcome = outcome;
        t.unit_id = Some("u-abc-1".to_string());
        t.name = Some("issue-1".to_string());
        t.kind = "single".to_string();
        t
    }

    #[test]
    fn exit_codes_follow_the_report() {
        let ok = CloseReport {
            targets: vec![target(CloseOutcome::Closed)],
            ..CloseReport::default()
        };
        assert_eq!(exit_code(&ok), EXIT_OK);
        let partial = CloseReport {
            targets: vec![
                target(CloseOutcome::Closed),
                target(CloseOutcome::PartiallyClosed),
            ],
            ..CloseReport::default()
        };
        assert_eq!(exit_code(&partial), EXIT_REFUSED);
        let whole = CloseReport {
            refused: Some(CloseRefusal {
                reason: CloseRefusalReason::NotAttested,
                message: "no".into(),
            }),
            ..CloseReport::default()
        };
        assert_eq!(exit_code(&whole), EXIT_REFUSED);
        let preview = CloseReport {
            dry_run: true,
            targets: vec![target(CloseOutcome::Listed)],
            ..CloseReport::default()
        };
        assert_eq!(exit_code(&preview), EXIT_OK);
    }

    #[test]
    fn json_carries_closed_refused_and_worktrees() {
        let mut closed = target(CloseOutcome::Closed);
        closed.forced_over = vec![CloseRefusalReason::Busy];
        closed.branch = Some("agent/dispatch-issue-1".into());
        closed.worktree_verdict = Some(WorktreeVerdict {
            path: "/wt".into(),
            verdict: WorktreeVerdictKind::KeptDirty,
        });
        let mut refused = target(CloseOutcome::Refused);
        refused.reason = Some(CloseRefusalReason::NotReported);
        refused.message = Some("had not reported".into());
        let report = CloseReport {
            targets: vec![closed, refused],
            ..CloseReport::default()
        };
        let json = render_json(&report, None);
        assert_eq!(json["closed"][0]["name"], "issue-1");
        assert_eq!(json["closed"][0]["forced_over"][0], "busy");
        assert_eq!(json["refused"][0]["reason"], "not-reported");
        assert_eq!(json["worktrees"][0]["verdict"], "kept-dirty");
        assert_eq!(json["targets"][1]["outcome"], "refused");
        assert_eq!(json["exit_code"], 1);
    }

    #[test]
    fn human_output_escapes_names_and_marks_forced_closes() {
        let mut closed = target(CloseOutcome::Closed);
        closed.name = Some("evil\nname\u{202e}".into());
        closed.forced_over = vec![CloseRefusalReason::Busy];
        closed.panes = vec![ClosePane {
            agent_id: "7".into(),
            pane_id: Some("p1".into()),
            role: None,
            is_orchestrator: false,
            status: Some("Working".into()),
            exited: false,
            stopped: Some(true),
        }];
        let text = render_human(
            &CloseReport {
                targets: vec![closed],
                ..CloseReport::default()
            },
            None,
        );
        assert!(text.contains("closed:"), "{text}");
        assert!(text.contains("(forced: was Working)"), "{text}");
        assert!(
            !text.contains('\u{202e}'),
            "bidi override must be escaped: {text}"
        );
        assert!(text.contains("evil\\nname"), "{text}");
    }

    /// Scenario: `--unit-id` names units by the ids a listing printed and
    /// sends them as the id selector, alone or with several ids; combined with
    /// any other selector it is refused before anything is sent.
    #[test]
    fn unit_ids_select_by_id_and_exclude_every_other_selector() {
        let args = CloseArgs {
            unit_ids: vec!["u-abc-1".into(), "u-abc-2".into()],
            ..CloseArgs::default()
        };
        assert_eq!(
            args.selector(),
            Ok(CloseSelector::UnitIds {
                ids: vec!["u-abc-1".into(), "u-abc-2".into()]
            })
        );
        for other in [
            CloseArgs {
                units: vec!["a".into()],
                ..args.clone()
            },
            CloseArgs {
                pane: Some("p".into()),
                ..args.clone()
            },
            CloseArgs {
                orchestration_of: Some("p".into()),
                ..args.clone()
            },
            CloseArgs {
                all: true,
                ..args.clone()
            },
        ] {
            assert!(other.selector().is_err(), "{other:?}");
        }
    }

    /// Scenario: a shell carrying part of a deck pane's identity runs `close
    /// --json`; the refusal is printed as JSON on stdout as well as on stderr,
    /// with exit status 1.
    #[test]
    fn an_identity_refusal_honours_json() {
        let out = identity_error_output(true, "refused: partial identity".into());
        assert_eq!(out.code, EXIT_REFUSED);
        let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        assert_eq!(value["error"], "refused: partial identity");
        assert_eq!(value["exit_code"], 1);
        assert!(out.stderr.contains("partial identity"));
        assert!(identity_error_output(false, "x".into()).stdout.is_empty());
    }

    #[test]
    fn the_selector_needs_exactly_one_spelling() {
        let mut args = CloseArgs::default();
        assert!(args.selector().is_err());
        args.units = vec!["a".into()];
        assert!(matches!(args.selector(), Ok(CloseSelector::Units(_))));
        args.all = true;
        assert!(args.selector().is_err());
        args.units.clear();
        assert_eq!(args.selector(), Ok(CloseSelector::AllUnits));
    }
}
