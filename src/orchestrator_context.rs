//! Orchestrator context composition, shared by BOTH spawn paths (PRD #220 / #222).
//!
//! These two functions used to live in `src/ui.rs`, where they had exactly one
//! caller: the interactive `Ctrl+n` new-pane path. The daemon spawn path
//! (`src/spawn.rs`) never called them, so a daemon-started orchestration — a PRD
//! #220 `dispatch`, or a PRD #120 scheduled issue-dispatch — came up with its
//! orchestrator never told that it IS an orchestrator, which roles exist, or how
//! to `delegate`. The orchestrator acted on its task alone and every worker sat
//! idle waiting for a delegation that could not arrive.
//!
//! Both functions were already PURE (config in, `String`/`fs` out, no UI state),
//! so making the daemon path reach parity is a MOVE, not a second
//! implementation — which is the whole point: two implementations of "start a
//! line of work" is what produced the gap.

use crate::project_config::OrchestrationConfig;

// ---------------------------------------------------------------------------
// Orchestrator prompt construction
// ---------------------------------------------------------------------------

/// Build the orchestrator context file content.
/// Includes the role's own prompt_template, the available-agents list, and
/// delegation protocol instructions.
pub fn build_orchestrator_context(config: &OrchestrationConfig) -> String {
    let mut content = String::new();

    // 1. Orchestrator's own prompt_template.
    if let Some(start_role) = config.roles.iter().find(|r| r.start)
        && let Some(ref tpl) = start_role.prompt_template
    {
        content.push_str(tpl);
        content.push_str("\n\n");
    }

    // 2. Available agents list.
    content.push_str("## Available agents\n\n");
    for role in &config.roles {
        if role.start {
            continue;
        }
        let desc = role.description.as_deref().unwrap_or("(no description)");
        content.push_str(&format!("- **{}**: {}\n", role.name, desc));
    }

    // 3. Delegation protocol.
    //
    // Issue #303: the task text reaches this CLI through YOUR shell, so
    // `--task "…"` is rewritten before argv is built — backticks and `$(…)` are
    // executed, `$VAR` substituted, an unescaped `"` ends the argument, a `\`
    // removes itself — while the delegation still reports success. The file form
    // is therefore the unconditional default here, with the reason stated inline
    // (an orchestrator that does not know WHY drifts back to `--task`).
    //
    // The audit of the first cut (auditor finding 1) showed that protecting only
    // the final `--task-file` read is not enough: an `echo "…"` expands the
    // content BEFORE it reaches disk, and an unquoted path can itself carry
    // command substitution or `..` traversal. Hence the four creation rules, and
    // the persistence/secrets note (#329's advice half).
    //
    // Round 3 then deleted the shell fallback that round 2 had recommended. A
    // quoted `<<'EOF'` delimiter disables expansion inside the heredoc, but a
    // task line that is exactly `EOF` terminates it and Bash parses and executes
    // every line after it — and task files are exactly where untrusted text
    // (issue bodies, code, another agent's brief) lands. "Use a fresh
    // unpredictable delimiter and check the payload for it" is a rule an agent
    // must get right on every single input, with silent command execution as the
    // failure mode, so the only recommendation left is a non-shell file writer.
    //
    // Round 4 restored the *inline* fallback — not the shell one. Round 3's
    // premise, "every agent in this system has a file-writing tool", confused
    // having a tool with being authorized to use it: the e2e gate then caught a
    // real Haiku worker launched with `--allowedTools Bash Read` calling `Write`
    // and parking forever on the approval prompt. Guidance that depends on an
    // unguaranteed permission produces exactly the silent stall #303 is about,
    // so all three branches (file / short plain inline / say you cannot) are now
    // stated outright rather than left to inference.
    let bin = crate::platform::paths::binary_name();
    content.push_str("\n## Delegation protocol\n\n");
    content.push_str(&format!(
        "To delegate work to an agent, use `delegate` with one command per agent. \
         Pass the task as a **file** — `--task-file` is the default, not an escape hatch:\n\n\
         ```bash\n\
         {bin} delegate --to <role-name> --task-file '.dot-agent-deck/<task-slug>.md'\n\
         ```\n\n\
         Four rules for producing that file. The last two are about the *path*, not the \
         contents:\n\n\
         - Write it with your **file-writing tool**. Do not construct it with shell redirection \
         or a heredoc: a line of the task text can terminate the heredoc, and everything after \
         that line is then executed as shell commands.\n\
         - Invent a **fresh slug** for `<task-slug>` from `[a-z0-9][a-z0-9-]*` only, at most 40 \
         characters. Never build it out of an issue title, a branch name, or any other text you \
         did not write yourself.\n\
         - No `/`, no `\\` and no `..` in the slug — the file goes directly in \
         `.dot-agent-deck/`.\n\
         - **Single-quote the whole path** in every command you run.\n\n\
         Task and summary files persist on disk after the handoff. Keep credentials, customer \
         data and other secrets out of them, pick a path that does not already exist, and delete \
         exactly that path once the handoff has succeeded.\n\n\
         **If you have no file-writing tool, or it is not authorized and invoking it would stop \
         you at an approval prompt, do not wait there — skip the file and use the inline form \
         below.** Never substitute shell redirection or a heredoc for the missing tool.\n\n\
         `--task \"…\"` is the fallback for exactly that case, and is safe only when the whole \
         task is **a single line of plain text with no backticks, no `$`, no `\"`, no `\\` and no \
         `!`**:\n\n\
         ```bash\n\
         {bin} delegate --to <role-name> --task \"Short plain task description.\"\n\
         ```\n\n\
         Why the allowlist is that narrow: everything after `--task` is processed by **your own \
         shell** before {bin} receives it. Backticks and `$(…)` are executed and \
         replaced by their output — usually empty — `$VAR` becomes its value or nothing, a \
         balanced inner `\"` is removed and changes how the rest of the argument is quoted, a \
         `\\` before `$`, a backtick, `\"` or `\\` removes itself, and a `\\` at the end of a \
         line removes itself *and* the newline. `!` is excluded because a Bash with history \
         expansion on rewrites it before argv is built. An unmatched `\"` aborts the command \
         outright; everything else is dropped silently while the delegation still reports \
         success, so the worker acts on a task with pieces missing and nobody sees an error. \
         `--task-file` is read from disk verbatim, so none of this applies to it.\n\n\
         If a task will not fit that one plain line and you cannot write a file, say so plainly \
         to the user and ask for the file-writing tool to be authorized, rather than improvising \
         a way around the allowlist.\n\n\
         To delegate to multiple agents in parallel, make **one call per agent** so each gets its own task:\n\n\
         ```bash\n\
         {bin} delegate --to coder --task-file '.dot-agent-deck/login-endpoint-coder.md'\n\
         {bin} delegate --to reviewer --task-file '.dot-agent-deck/login-endpoint-reviewer.md'\n\
         ```\n\n\
         If all agents should receive the **exact same task**, you may combine them in one call:\n\n\
         ```bash\n\
         {bin} delegate --to <role1> --to <role2> --task-file '.dot-agent-deck/<task-slug>.md'\n\
         ```\n\n\
         When all work is complete and you are satisfied with the results:\n\n\
         ```bash\n\
         {bin} work-done --done --task-file '.dot-agent-deck/final-summary-<summary-slug>.md'\n\
         ```\n\
         (or `{bin} work-done --done --task \"Final summary.\"` when that summary really is \
         one plain line). The same four rules apply to that file: `<summary-slug>` is a fresh slug \
         you invent, the path must not already exist before you write it, and you delete exactly \
         that path once the command has exited successfully.\n\n\
         **Shell safety and context length are two different problems.** Writing long context to \
         `.dot-agent-deck/<task-slug>.md` and *referencing that path inside* `--task \"…\"` keeps the \
         task description short, but the description itself still goes through your shell. Passing \
         the file with `--task-file` is what keeps the shell out of the text. One file solves both \
         at once: write the full task to `.dot-agent-deck/<task-slug>.md` and hand it over with \
         `--task-file`.\n"
    ));

    // 4. Important guidelines.
    content.push_str(&format!(
        "\n## Important\n\n\
         Wait for the user to tell you what to work on.\n\n\
         Once you know the task, delegate immediately via the CLI commands above. \
         Do NOT ask for confirmation before delegating. \
         Do NOT offer to design, analyze, or plan — that is the workers' job. \
         Do NOT ask 'should I proceed?' or 'do you want me to delegate?' — just delegate. \
         Your only job: understand what needs doing, frame clear task descriptions, and hand off.\n\n\
         Never send a new task to a worker that is still working on a previous task. \
         Wait for its work-done signal before delegating again to the same worker. \
         Delegating to different workers in parallel is fine.\n\n\
         Delegation is one-way: orchestrator → worker. Workers NEVER delegate to other workers \
         — a `{bin} delegate` call from inside a worker does not route back through your \
         notification stream, so the downstream task is silently dropped and the calling worker \
         waits forever (or signals work-done in a paused state). When briefing a worker, never \
         instruct them to \"delegate the fix to coder\" or \"hand off to <other role>\". \
         Instead, tell them to report the diagnosis back and signal work-done; you (the orchestrator) \
         will delegate the next hop. The chain you coordinate is: worker A diagnoses → reports → \
         you delegate to worker B → worker B works → reports → you re-engage worker A.\n\n\
         When a task related to a PRD is fully completed (all workers done, reviews passed), \
         run `/prd-update-progress` yourself before signaling `--done` or moving to the next task.\n"
    ));

    content
}

/// Write the orchestrator context to a file and return a one-liner to inject.
/// Multi-line prompts don't submit in Claude Code via PTY, so we use a file reference.
///
/// `task` is the caller's own instruction, if any — a PRD #220 `dispatch --task`
/// or a PRD #120 per-issue prompt. It is folded INTO the context file rather than
/// concatenated onto the returned line, for the same reason the context itself is
/// a file: a multi-line prompt does not submit reliably through a PTY, and task
/// text is arbitrary (an issue body, a brief written by another agent). So the
/// orchestrator receives one line, and everything it needs is on disk.
///
/// `None` reproduces the pre-#222 output byte-for-byte, which is what keeps the
/// interactive `Ctrl+n` path unchanged.
pub fn prepare_orchestrator_prompt(
    config: &OrchestrationConfig,
    cwd: &str,
    task: Option<&str>,
) -> Option<String> {
    let dir = std::path::Path::new(cwd).join(".dot-agent-deck");
    std::fs::create_dir_all(&dir).ok()?;
    let file_path = dir.join("orchestrator-context.md");
    let mut content = build_orchestrator_context(config);
    let task = task.map(str::trim).filter(|t| !t.is_empty());
    if let Some(task) = task {
        content.push_str("\n## Your task\n\n");
        content.push_str(task);
        content.push('\n');
    }
    std::fs::write(&file_path, &content).ok()?;
    // With a task, the closing instruction must NOT be "wait for instructions" —
    // the instruction is already in the file, and telling the orchestrator to wait
    // is what would leave a dispatched unit idle forever.
    Some(if task.is_some() {
        "Read .dot-agent-deck/orchestrator-context.md for your role, the available agents, the \
         delegation protocol, and your task under `## Your task`. Then carry out that task, \
         delegating to the agents listed there."
            .to_string()
    } else {
        "Read .dot-agent-deck/orchestrator-context.md for your role, available agents, and \
         delegation protocol. Acknowledge your role and wait for instructions."
            .to_string()
    })
}

/// The exact separator `prepare_orchestrator_prompt` writes ahead of a task —
/// matched here rather than duplicated as a shared constant, since this is
/// the only other reader.
const TASK_SECTION_MARKER: &str = "\n## Your task\n\n";

/// Read an existing orchestrator context file's own `## Your task` section
/// back off disk, if any. `None` covers every case where there is nothing to
/// carry forward: the file does not exist yet, cannot be read, or was written
/// with no task (the interactive `Ctrl+n` path, which never carries one).
///
/// Exists so a re-assertion (compaction or `/clear`) can re-supply the SAME
/// task `prepare_orchestrator_prompt` would otherwise silently drop — see
/// [`reassert_orchestrator_prompt`].
fn read_back_task(cwd: &str) -> Option<String> {
    let file_path = std::path::Path::new(cwd)
        .join(".dot-agent-deck")
        .join("orchestrator-context.md");
    let content = std::fs::read_to_string(file_path).ok()?;
    let after = content.split_once(TASK_SECTION_MARKER)?.1;
    let task = after.trim();
    (!task.is_empty()).then(|| task.to_string())
}

/// Re-run `prepare_orchestrator_prompt` for a re-assertion (compaction or
/// `/clear`), preserving whatever task the existing context file already
/// carries instead of silently discarding it.
///
/// Before this, both re-arm sites in `src/ui.rs` called
/// `prepare_orchestrator_prompt(config, cwd, None)` directly — correct for the
/// interactive `Ctrl+n` orchestrator, which never has a task, but wrong for a
/// `dispatch --task` or per-issue orchestration (`src/spawn.rs`): a
/// compaction or `/clear` on one of those rewrote the file with no `## Your
/// task` section at all and delivered the no-task "wait for instructions"
/// pointer over a task that was actively in progress, deleting it from disk
/// and telling the orchestrator to stop rather than continue.
///
/// Reading the task back off the file the daemon itself just wrote is
/// non-destructive and needs no new tab state — `Tab::Orchestration` does not
/// need to start carrying the task alongside `config`/`cwd` for this to work,
/// because the file already has it.
pub fn reassert_orchestrator_prompt(config: &OrchestrationConfig, cwd: &str) -> Option<String> {
    let task = read_back_task(cwd);
    prepare_orchestrator_prompt(config, cwd, task.as_deref())
}

// ---------------------------------------------------------------------------
// M6: Skill file auto-deployment
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::OrchestrationRoleConfig;
    use spec::spec;

    fn role(
        name: &str,
        start: bool,
        tpl: Option<&str>,
        desc: Option<&str>,
    ) -> OrchestrationRoleConfig {
        OrchestrationRoleConfig {
            agent: None,
            name: name.to_string(),
            command: "cat".to_string(),
            start,
            description: desc.map(str::to_string),
            prompt_template: tpl.map(str::to_string),
            clear: false,
        }
    }

    fn config() -> OrchestrationConfig {
        OrchestrationConfig {
            default: false,
            name: "digest".to_string(),
            roles: vec![
                role("orchestrator", true, Some("You lead the team."), None),
                role("coder", false, None, Some("Implements features")),
                role("reviewer", false, None, Some("Reviews changes")),
            ],
        }
    }

    /// The context a daemon-spawned orchestration was missing entirely: the
    /// orchestrator's own template, every worker by name, and how to delegate.
    #[test]
    fn context_carries_the_template_the_agents_and_the_delegation_protocol() {
        let c = build_orchestrator_context(&config());
        assert!(
            c.contains("You lead the team."),
            "orchestrator's own template"
        );
        assert!(c.contains("coder") && c.contains("Implements features"));
        assert!(c.contains("reviewer") && c.contains("Reviews changes"));
        assert!(c.contains("delegate"), "the delegation protocol");
        assert!(
            !c.contains("**orchestrator**:"),
            "the start role is the reader, not one of its own available agents"
        );
    }

    /// With a caller task (PRD #220 `dispatch --task`, PRD #120 per-issue prompt)
    /// the task rides INSIDE the file and the one-line pointer tells the
    /// orchestrator to CARRY IT OUT.
    ///
    /// The closing sentence matters as much as the task: the no-task form says
    /// "wait for instructions", and leaving that in place is what would strand a
    /// dispatched unit idle forever with its task sitting unread on disk.
    #[test]
    fn a_caller_task_lands_in_the_file_and_the_pointer_says_carry_it_out() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        let line = prepare_orchestrator_prompt(&config(), &cwd, Some("Verify PR #232 and report."))
            .expect("context file written");
        assert!(
            !line.contains('\n'),
            "the injected prompt must be ONE line: {line:?}"
        );
        assert!(
            line.contains("carry out that task"),
            "with a task the pointer must direct action, got {line:?}"
        );
        assert!(
            !line.contains("wait for instructions"),
            "a dispatched orchestrator told to wait would sit idle forever: {line:?}"
        );

        let written =
            std::fs::read_to_string(tmp.path().join(".dot-agent-deck/orchestrator-context.md"))
                .expect("context file on disk");
        assert!(written.contains("## Your task"));
        assert!(written.contains("Verify PR #232 and report."));
        // The protocol is still there — the task is additive, not a replacement.
        assert!(written.contains("delegate"));
        assert!(written.contains("You lead the team."));
    }

    /// `None` keeps the interactive `Ctrl+n` path byte-for-byte unchanged.
    #[test]
    fn no_task_reproduces_the_pre_parity_prompt_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        let line = prepare_orchestrator_prompt(&config(), &cwd, None).expect("written");
        assert!(line.contains("Acknowledge your role and wait for instructions."));
        let written =
            std::fs::read_to_string(tmp.path().join(".dot-agent-deck/orchestrator-context.md"))
                .unwrap();
        assert_eq!(
            written,
            build_orchestrator_context(&config()),
            "with no task the file must be exactly the composed context"
        );
        assert!(!written.contains("## Your task"));
    }

    /// A blank or whitespace-only task is treated as absent rather than emitting an
    /// empty `## Your task` section and telling the orchestrator to act on nothing.
    #[test]
    fn a_blank_task_is_treated_as_no_task() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();
        for blank in [Some(""), Some("   \n  ")] {
            let line = prepare_orchestrator_prompt(&config(), &cwd, blank).expect("written");
            assert!(line.contains("wait for instructions"), "got {line:?}");
            let written =
                std::fs::read_to_string(tmp.path().join(".dot-agent-deck/orchestrator-context.md"))
                    .unwrap();
            assert!(!written.contains("## Your task"));
        }
    }

    /// Regression for the maintainer review on the fork's upstream PR #789
    /// "Required 1": both `src/ui.rs` re-arm sites used to call
    /// `prepare_orchestrator_prompt(config, cwd, None)` directly, which wiped
    /// a dispatched task's `## Your task` section on every compaction/`/clear`
    /// re-assertion and told the orchestrator to wait rather than continue.
    /// `reassert_orchestrator_prompt` must read that section back and carry
    /// it forward instead.
    #[test]
    fn reassert_preserves_an_existing_dispatched_task() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        // Simulate the spawn-time write a `dispatch --task` orchestration
        // (`src/spawn.rs`) leaves on disk.
        prepare_orchestrator_prompt(&config(), &cwd, Some("Verify PR #232 and report."))
            .expect("spawn-time write");

        let line = reassert_orchestrator_prompt(&config(), &cwd).expect("re-assertion written");
        assert!(
            line.contains("carry out that task"),
            "a re-assertion that found an existing task must still direct action, got {line:?}"
        );
        assert!(
            !line.contains("wait for instructions"),
            "must not tell a dispatched orchestrator to wait: {line:?}"
        );

        let written =
            std::fs::read_to_string(tmp.path().join(".dot-agent-deck/orchestrator-context.md"))
                .expect("context file on disk");
        assert!(
            written.contains("Verify PR #232 and report."),
            "the task must survive the re-assertion rewrite:\n{written}"
        );
    }

    /// The interactive `Ctrl+n` orchestrator never has a task, so a
    /// re-assertion on it must reproduce today's no-task behavior exactly —
    /// `reassert_orchestrator_prompt` must not invent one.
    #[test]
    fn reassert_with_no_prior_task_reproduces_no_task_behavior() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        prepare_orchestrator_prompt(&config(), &cwd, None).expect("spawn-time write");

        let line = reassert_orchestrator_prompt(&config(), &cwd).expect("re-assertion written");
        assert!(line.contains("wait for instructions"), "got {line:?}");

        let written =
            std::fs::read_to_string(tmp.path().join(".dot-agent-deck/orchestrator-context.md"))
                .unwrap();
        assert!(!written.contains("## Your task"));
    }

    /// With no context file on disk at all (a re-assertion racing ahead of any
    /// spawn-time write, or a pruned file), `reassert_orchestrator_prompt`
    /// must fall back to the ordinary no-task write rather than failing —
    /// `read_back_task` returns `None` and `prepare_orchestrator_prompt`
    /// creates the file fresh, matching `prepare_orchestrator_prompt`'s own
    /// `None` behavior.
    #[test]
    fn reassert_with_no_existing_file_falls_back_to_no_task() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_string_lossy().to_string();

        let line = reassert_orchestrator_prompt(&config(), &cwd).expect("written from scratch");
        assert!(line.contains("wait for instructions"), "got {line:?}");
    }

    /// Scenario: Build the orchestrator context and check that its `delegate`
    /// and `work-done` command examples name what `binary_name()` resolves
    /// for the running process — under `cargo test` the throwaway test binary
    /// is never on `$PATH`, so this is its own absolute `current_exe()` path,
    /// never the crate's baked-in literal name.
    #[spec("orchestration/delegate/016")]
    #[test]
    fn delegate_016_orchestrator_context_names_the_running_binary() {
        let c = build_orchestrator_context(&config());
        let bin = crate::platform::paths::binary_name();

        assert_ne!(
            bin, "dot-agent-deck",
            "this test only proves anything when the test binary's own file name differs \
             from the literal the pre-fix code always emitted"
        );
        assert!(
            c.contains(&format!("{bin} delegate --to")),
            "the delegate examples must name the running binary ({bin:?}), got: {c}"
        );
        assert!(
            c.contains(&format!("{bin} work-done --done")),
            "the work-done examples must name the running binary ({bin:?}), got: {c}"
        );
        // Reviewer finding F6: pin the ABSENCE of the old literal too, so a
        // later edit that reintroduces a hardcoded `dot-agent-deck` example
        // fails this test instead of staying green alongside the dynamic one.
        assert!(
            !c.contains("dot-agent-deck delegate --to"),
            "a hardcoded literal must not appear in the delegate examples, got: {c}"
        );
        assert!(
            !c.contains("dot-agent-deck work-done --done"),
            "a hardcoded literal must not appear in the work-done examples, got: {c}"
        );
    }
}
