//! PRD #1885: `dot-agent-deck trust list | approve | revoke`.
//!
//! The CLI half of the approval store in [`crate::project_trust`]. It reads and
//! writes the store directly, so it works whether or not a daemon is running,
//! and the daemon sees the change on its next check because every check reads
//! the store again.
//!
//! `approve` is the one verb here that grants anything, so it is the one that
//! refuses inside a pane the deck started: an agent there could otherwise
//! approve its own project's commands on the user's behalf. That refusal reads
//! the deck's pane-identity environment ([`in_deck_pane`]); a same-user process
//! that drops that environment can still run it, which is control-plane
//! scoping's to close (#1886), not this verb's.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::project_trust::{
    self, ApproveOutcome, RoleChange, TrustCheck, TrustStore, TrustVerdict,
};
use crate::untrusted_text::escape_control_and_bidi;

/// The question `approve` asks on a terminal.
pub const APPROVE_QUESTION: &str = "Approve these commands and prompts? [y/N] ";

/// What `approve` says inside a deck pane.
pub const IN_PANE_REFUSAL: &str = "dot-agent-deck trust approve refuses to run inside a pane the \
     deck started, so an agent cannot approve commands on your behalf: run it from a terminal \
     outside the deck, or approve from the deck's own prompt (choose the orchestration in the \
     New Agent form).";

/// The environment variables the deck sets on every pane it starts. Any of
/// them present and non-empty means "inside a deck pane".
pub const PANE_IDENTITY_ENV: &[&str] = &[
    crate::agent_pty::DOT_AGENT_DECK_PANE_ID,
    crate::hook_provenance::DOT_AGENT_DECK_PANE_CAPABILITY,
    crate::agent_pty::DOT_AGENT_DECK_AGENT_ID,
];

/// Whether this process runs inside a pane the deck started.
pub fn in_deck_pane() -> bool {
    PANE_IDENTITY_ENV
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

/// How the process was started, for `approve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApproveContext {
    /// Inside a deck pane ([`in_deck_pane`]).
    pub in_deck_pane: bool,
    /// Stdin and stdout are both terminals, so the question can be answered.
    pub interactive: bool,
}

/// `trust list`: one line per stored approval. Answers whether it succeeded.
pub fn list(store: &TrustStore, out: &mut dyn Write, err: &mut dyn Write) -> bool {
    let approvals = match store.load() {
        Ok(approvals) => approvals,
        Err(e) => {
            let _ = writeln!(
                err,
                "Could not read the trust store at {}: {e}",
                store.file_path().display()
            );
            return false;
        }
    };
    if approvals.approvals.is_empty() {
        let _ = writeln!(out, "No approved orchestrations.");
        return true;
    }
    for approval in &approvals.approvals {
        let state = list_state(approval);
        let _ = writeln!(
            out,
            "{}  {}  approved {}  {state}",
            escape_control_and_bidi(&approval.project.display().to_string()),
            escape_control_and_bidi(&approval.orchestration),
            escape_control_and_bidi(&approval.approved_at),
        );
    }
    true
}

/// `current`, `changed` or `missing` for one stored approval, read from the
/// config at its project path.
fn list_state(approval: &project_trust::Approval) -> &'static str {
    let Ok(config) = project_trust::read_config(&approval.project) else {
        return "missing";
    };
    match config
        .orchestrations
        .iter()
        .find(|o| o.name == approval.orchestration)
    {
        None => "missing",
        Some(orch) if project_trust::role_texts(orch) == approval.roles => "current",
        Some(_) => "changed",
    }
}

/// `trust approve [<dir>] [--orchestration <name>] [--yes]`. Answers whether it
/// succeeded (an answer of "no" to the question is not a success).
#[allow(clippy::too_many_arguments)]
pub fn approve(
    store: &TrustStore,
    dir: &Path,
    orchestration: Option<&str>,
    yes: bool,
    context: ApproveContext,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    if context.in_deck_pane {
        let _ = writeln!(err, "{IN_PANE_REFUSAL}");
        return false;
    }
    let checks = match checks_for(store, dir, orchestration) {
        Ok(checks) => checks,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return false;
        }
    };
    let pending: Vec<&TrustCheck> = checks.iter().filter(|c| !c.verdict.is_approved()).collect();
    for check in &checks {
        print_check(check, out);
    }
    if pending.is_empty() {
        let _ = writeln!(
            out,
            "Nothing to approve: every orchestration above is approved and unchanged."
        );
        return true;
    }
    if !yes {
        if !context.interactive {
            let _ = writeln!(
                err,
                "Not approved: there is no terminal to ask on. Review the text above and run \
                 again with --yes to approve it."
            );
            return false;
        }
        let _ = write!(out, "{APPROVE_QUESTION}");
        let _ = out.flush();
        let mut answer = String::new();
        if input.read_line(&mut answer).is_err() {
            let _ = writeln!(err, "Not approved: could not read an answer.");
            return false;
        }
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            let _ = writeln!(out, "Not approved. Nothing was changed.");
            return false;
        }
    }
    let mut ok = true;
    for check in pending {
        match project_trust::approve_if_unchanged(
            store,
            &check.config_dir,
            &check.orchestration,
            &check.digest(),
        ) {
            Ok(ApproveOutcome::Approved) => {
                let _ = writeln!(
                    out,
                    "Approved orchestration '{}' of project {}.",
                    escape_control_and_bidi(&check.orchestration),
                    escape_control_and_bidi(&check.project.display().to_string())
                );
            }
            Ok(ApproveOutcome::Stale) => {
                ok = false;
                let _ = writeln!(
                    err,
                    "Not approved: orchestration '{}' changed while you were reviewing it. Run \
                     the command again to review the new text.",
                    escape_control_and_bidi(&check.orchestration)
                );
            }
            Err(e) => {
                ok = false;
                let _ = writeln!(
                    err,
                    "Not approved: orchestration '{}': {e}",
                    escape_control_and_bidi(&check.orchestration)
                );
            }
        }
    }
    ok
}

/// `trust revoke [<dir>] [--orchestration <name>]`. Answers whether anything
/// was revoked.
pub fn revoke(
    store: &TrustStore,
    dir: &Path,
    orchestration: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let project = project_trust::project_key_for_revoke(dir);
    let removed = store.update(|approvals| {
        let removed = approvals.revoke(&project, orchestration);
        (removed, removed > 0)
    });
    let shown = escape_control_and_bidi(&project.display().to_string());
    match removed {
        Ok(0) => {
            let what = match orchestration {
                Some(name) => format!(
                    "orchestration '{}' of project {shown}",
                    escape_control_and_bidi(name)
                ),
                None => format!("project {shown}"),
            };
            let _ = writeln!(err, "Nothing to revoke: no approval is stored for {what}.");
            false
        }
        Ok(n) => {
            let _ = writeln!(
                out,
                "Revoked {n} approval{} of project {shown}.",
                if n == 1 { "" } else { "s" }
            );
            true
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "Could not update the trust store at {}: {e}",
                store.file_path().display()
            );
            false
        }
    }
}

/// The checks `approve` covers: the named orchestration, or every one in the
/// config.
fn checks_for(
    store: &TrustStore,
    dir: &Path,
    orchestration: Option<&str>,
) -> Result<Vec<TrustCheck>, String> {
    let dir: PathBuf = std::path::absolute(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let config = project_trust::read_config(&dir).map_err(|e| e.to_string())?;
    let names = match orchestration {
        Some(name) => vec![name.to_string()],
        None => project_trust::orchestration_names(&config),
    };
    if names.is_empty() {
        return Err(format!(
            "{} declares no orchestrations, so there is nothing to approve.",
            dir.join(crate::project_config::CONFIG_FILE_NAME).display()
        ));
    }
    names
        .iter()
        .map(|name| project_trust::inspect(store, &dir, name).map_err(|e| e.to_string()))
        .collect()
}

/// Print everything an approval of `check` would record.
fn print_check(check: &TrustCheck, out: &mut dyn Write) {
    let state = match &check.verdict {
        TrustVerdict::Approved => "approved, unchanged",
        TrustVerdict::NeverApproved => "not approved before",
        TrustVerdict::Changed { .. } => "changed since you approved it",
    };
    let _ = writeln!(
        out,
        "Project: {}",
        escape_control_and_bidi(&check.project.display().to_string())
    );
    let _ = writeln!(
        out,
        "Orchestration: {} ({state})",
        escape_control_and_bidi(&check.orchestration)
    );
    for role in &check.roles {
        let mark = match check.verdict.change_of(&role.name) {
            Some(RoleChange::Changed) => " [changed]",
            Some(RoleChange::New) => " [new]",
            None => "",
        };
        let _ = writeln!(out, "  Role: {}{mark}", escape_control_and_bidi(&role.name));
        let _ = writeln!(
            out,
            "    command: {}",
            escape_control_and_bidi(&role.command)
        );
        if let Some(agent) = &role.agent {
            let _ = writeln!(out, "    agent: {}", escape_control_and_bidi(agent));
        }
        if role.start {
            let _ = writeln!(out, "    start: true");
        }
        if !role.clear {
            let _ = writeln!(out, "    clear: false");
        }
        print_text(out, "description", role.description.as_deref());
        print_text(out, "prompt_template", role.prompt_template.as_deref());
    }
    if let TrustVerdict::Changed { removed, .. } = &check.verdict
        && !removed.is_empty()
    {
        let names: Vec<String> = removed.iter().map(|r| escape_control_and_bidi(r)).collect();
        let _ = writeln!(out, "  Removed since you approved it: {}", names.join(", "));
    }
    let _ = writeln!(out);
}

/// A possibly multi-line field, every line escaped and indented.
fn print_text(out: &mut dyn Write, label: &str, text: Option<&str>) {
    match text {
        None => {
            let _ = writeln!(out, "    {label}: (none)");
        }
        Some(text) if !text.contains('\n') => {
            let _ = writeln!(out, "    {label}: {}", escape_control_and_bidi(text));
        }
        Some(text) => {
            let _ = writeln!(out, "    {label}:");
            for line in text.lines() {
                let _ = writeln!(out, "      {}", escape_control_and_bidi(line));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[[orchestrations]]
name = "team"
[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = "Line one.\nLine two."
[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Writes code"
"#;

    struct Fixture {
        _scratch: tempfile::TempDir,
        store: TrustStore,
        project: PathBuf,
    }

    fn fixture() -> Fixture {
        let scratch = crate::test_temp::tempdir().unwrap();
        let project = scratch.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".dot-agent-deck.toml"), CONFIG).unwrap();
        Fixture {
            store: TrustStore::at(scratch.path().join("state")),
            project,
            _scratch: scratch,
        }
    }

    const OUTSIDE_TTY: ApproveContext = ApproveContext {
        in_deck_pane: false,
        interactive: true,
    };

    fn run_approve(
        f: &Fixture,
        yes: bool,
        context: ApproveContext,
        answer: &str,
    ) -> (bool, String, String) {
        let mut input = std::io::Cursor::new(answer.as_bytes().to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ok = approve(
            &f.store, &f.project, None, yes, context, &mut input, &mut out, &mut err,
        );
        (
            ok,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn approved(f: &Fixture) -> bool {
        project_trust::inspect(&f.store, &f.project, "team")
            .unwrap()
            .verdict
            .is_approved()
    }

    #[test]
    fn approve_prints_the_full_text_and_asks() {
        let f = fixture();
        let (ok, out, _) = run_approve(&f, false, OUTSIDE_TTY, "y\n");
        assert!(ok);
        for expected in [
            "Orchestration: team (not approved before)",
            "Role: orchestrator",
            "command: claude",
            "Line one.",
            "Line two.",
            "Role: coder",
            "command: devbox run agent-coder",
            "description: Writes code",
            APPROVE_QUESTION,
        ] {
            assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
        }
        assert!(approved(&f));
    }

    #[test]
    fn only_y_or_yes_approves() {
        for answer in ["\n", "n\n", "yess\n", "approve\n", ""] {
            let f = fixture();
            let (ok, out, _) = run_approve(&f, false, OUTSIDE_TTY, answer);
            assert!(!ok, "{answer:?} must not approve");
            assert!(!approved(&f), "{answer:?} must not approve");
            assert!(out.contains("Not approved"), "{out}");
        }
        for answer in ["y\n", "YES\n", " yes \n"] {
            let f = fixture();
            assert!(run_approve(&f, false, OUTSIDE_TTY, answer).0, "{answer:?}");
            assert!(approved(&f));
        }
    }

    #[test]
    fn without_a_terminal_it_refuses_unless_yes() {
        let f = fixture();
        let headless = ApproveContext {
            in_deck_pane: false,
            interactive: false,
        };
        let (ok, out, err) = run_approve(&f, false, headless, "y\n");
        assert!(!ok);
        assert!(err.contains("--yes"), "{err}");
        assert!(!out.contains(APPROVE_QUESTION));
        assert!(!approved(&f));

        let (ok, out, _) = run_approve(&f, true, headless, "");
        assert!(ok);
        assert!(
            out.contains("command: devbox run agent-coder"),
            "--yes still prints: {out}"
        );
        assert!(approved(&f));
    }

    #[test]
    fn inside_a_deck_pane_it_refuses_even_with_yes() {
        let f = fixture();
        let in_pane = ApproveContext {
            in_deck_pane: true,
            interactive: true,
        };
        let (ok, _, err) = run_approve(&f, true, in_pane, "y\n");
        assert!(!ok);
        assert!(
            err.contains("run it from a terminal outside the deck"),
            "{err}"
        );
        assert!(!approved(&f));
    }

    #[test]
    fn a_changed_role_is_marked() {
        let f = fixture();
        assert!(run_approve(&f, true, OUTSIDE_TTY, "").0);
        std::fs::write(
            f.project.join(".dot-agent-deck.toml"),
            CONFIG.replace("agent-coder", "agent-coder2")
                + "[[orchestrations.roles]]\nname = \"tester\"\ncommand = \"cat\"\n",
        )
        .unwrap();
        let (_, out, _) = run_approve(&f, false, OUTSIDE_TTY, "n\n");
        assert!(out.contains("changed since you approved it"), "{out}");
        assert!(out.contains("Role: coder [changed]"), "{out}");
        assert!(out.contains("Role: tester [new]"), "{out}");
        assert!(out.contains("Role: orchestrator\n"), "{out}");
    }

    #[test]
    fn escape_sequences_in_the_config_are_shown_not_obeyed() {
        let f = fixture();
        std::fs::write(
            f.project.join(".dot-agent-deck.toml"),
            CONFIG.replace(
                "command = \"claude\"",
                "command = \"rm -rf ~ \\u001b[2K\\rclaude\"",
            ),
        )
        .unwrap();
        let (_, out, _) = run_approve(&f, false, OUTSIDE_TTY, "n\n");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(out.contains("rm -rf ~"), "{out}");
    }

    #[test]
    fn list_shows_current_changed_and_missing() {
        let f = fixture();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(list(&f.store, &mut out, &mut err));
        assert!(String::from_utf8_lossy(&out).contains("No approved orchestrations"));

        assert!(run_approve(&f, true, OUTSIDE_TTY, "").0);
        let line = |f: &Fixture| {
            let mut out = Vec::new();
            assert!(list(&f.store, &mut out, &mut Vec::new()));
            String::from_utf8(out).unwrap()
        };
        let listed = line(&f);
        assert!(listed.contains("team"), "{listed}");
        assert!(listed.trim_end().ends_with("current"), "{listed}");

        std::fs::write(
            f.project.join(".dot-agent-deck.toml"),
            CONFIG.replace("Writes code", "Writes more code"),
        )
        .unwrap();
        assert!(line(&f).trim_end().ends_with("changed"));

        std::fs::remove_file(f.project.join(".dot-agent-deck.toml")).unwrap();
        assert!(line(&f).trim_end().ends_with("missing"));
    }

    #[test]
    fn revoke_removes_and_reports_when_there_is_nothing() {
        let f = fixture();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(!revoke(&f.store, &f.project, None, &mut out, &mut err));
        assert!(String::from_utf8_lossy(&err).contains("Nothing to revoke"));

        assert!(run_approve(&f, true, OUTSIDE_TTY, "").0);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert!(!revoke(
            &f.store,
            &f.project,
            Some("nope"),
            &mut out,
            &mut err
        ));
        assert!(approved(&f));
        assert!(revoke(
            &f.store,
            &f.project,
            Some("team"),
            &mut out,
            &mut err
        ));
        assert!(!approved(&f));
    }

    #[test]
    fn revoke_works_for_a_project_that_is_gone() {
        let f = fixture();
        assert!(run_approve(&f, true, OUTSIDE_TTY, "").0);
        let key = project_trust::project_identity(&f.project).unwrap();
        std::fs::remove_dir_all(&f.project).unwrap();
        assert!(revoke(
            &f.store,
            &key,
            None,
            &mut Vec::new(),
            &mut Vec::new()
        ));
        assert!(f.store.load().unwrap().approvals.is_empty());
    }
}
