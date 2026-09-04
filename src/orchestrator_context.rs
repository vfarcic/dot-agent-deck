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

/// Fold the caller's own task, if any, into the composed context.
///
/// Split out from [`prepare_orchestrator_prompt`] in PRD #819 M4 so that
/// composing and *publishing* are two steps rather than one: the daemon needs
/// the composed bytes before they reach a filesystem, both to bound them
/// ([`MAX_CONTEXT_BYTES`]) and to hand them to
/// [`publish_orchestrator_context`]. The composition itself is unchanged — same
/// sections, same separator, same trimming rule — because this is the ONE
/// composer the desktop, the TUI and the daemon all share, and forking it is
/// what produced the parity gap PRD #222 closed.
///
/// `task` is expected already trimmed and non-empty when `Some`; callers go
/// through [`prepare_orchestrator_context`], which applies that rule once.
pub fn compose_orchestrator_context(config: &OrchestrationConfig, task: Option<&str>) -> String {
    let mut content = build_orchestrator_context(config);
    if let Some(task) = task {
        content.push_str(TASK_SECTION_MARKER);
        content.push_str(task);
        content.push('\n');
    }
    content
}

/// The one-liner injected into the coordinator's PTY, pointing at the file.
///
/// With a task, the closing instruction must NOT be "wait for instructions" —
/// the instruction is already in the file, and telling the orchestrator to wait
/// is what would leave a dispatched unit idle forever.
fn orchestrator_prompt_line(has_task: bool) -> String {
    if has_task {
        "Read .dot-agent-deck/orchestrator-context.md for your role, the available agents, the \
         delegation protocol, and your task under `## Your task`. Then carry out that task, \
         delegating to the agents listed there."
            .to_string()
    } else {
        "Read .dot-agent-deck/orchestrator-context.md for your role, available agents, and \
         delegation protocol. Acknowledge your role and wait for instructions."
            .to_string()
    }
}

/// What a successful [`prepare_orchestrator_context`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContext {
    /// The file the coordinator will read — the path the daemon reports back on
    /// [`crate::event::PreparedWorkflow::context_path`].
    pub context_path: std::path::PathBuf,
    /// The one-liner to inject into the coordinator's PTY.
    pub prompt: String,
}

/// Compose the orchestrator context and publish it, reporting **why** on
/// failure.
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
///
/// **Blocking.** Every async caller goes through
/// [`crate::project_resolve::run_bounded`].
pub fn prepare_orchestrator_context(
    config: &OrchestrationConfig,
    cwd: &std::path::Path,
    task: Option<&str>,
) -> Result<PreparedContext, ContextPublishError> {
    let task = task.map(str::trim).filter(|t| !t.is_empty());
    let content = compose_orchestrator_context(config, task);
    let context_path = publish_orchestrator_context(cwd, &content)?;
    Ok(PreparedContext {
        context_path,
        prompt: orchestrator_prompt_line(task.is_some()),
    })
}

/// Write the orchestrator context to a file and return a one-liner to inject.
/// Multi-line prompts don't submit in Claude Code via PTY, so we use a file reference.
///
/// The `Option` return is kept for the three pre-existing callers — the
/// interactive `Ctrl+n` path (`crate::ui`), the daemon spawn path
/// (`crate::spawn`) and the desktop's launch flow — each of which already has a
/// degraded behaviour for "no context file" and no way to act on a cause. The
/// cause is no longer *lost*, though: it is logged here, and a caller that needs
/// it calls [`prepare_orchestrator_context`] instead. PRD #819's daemon verb is
/// that caller.
pub fn prepare_orchestrator_prompt(
    config: &OrchestrationConfig,
    cwd: &str,
    task: Option<&str>,
) -> Option<String> {
    match prepare_orchestrator_context(config, std::path::Path::new(cwd), task) {
        Ok(prepared) => Some(prepared.prompt),
        Err(e) => {
            tracing::warn!(reason = %e, "could not publish the coordinator context");
            None
        }
    }
}

/// The exact separator [`compose_orchestrator_context`] writes ahead of a task.
///
/// One constant now written by the composer and read by [`read_back_task`],
/// rather than a literal in one place matched by a constant in the other — the
/// arrangement before PRD #819 M4 split the composer out.
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
        .join(CONTEXT_DIR_NAME)
        .join(CONTEXT_FILE_NAME);
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
// PRD #819 M4: the publish
//
// After M4 the DAEMON creates a directory and writes a file at a location
// derived from a client-supplied path, on an endpoint #741 will later make
// reachable off-box. The write primitive this replaced — `create_dir_all` plus
// `std::fs::write` — was fine for a path this process chose and is not fine for
// that: it followed a destination symlink, truncated in place, was not atomic,
// swallowed its cause behind an `Option`, and created the file under the
// ambient umask.
//
// Joining a fixed suffix does block a lexical `..` escape, and the daemon
// canonicalises the project root before it gets here — but canonicalisation
// removes the symlinks present at that MOMENT and protects neither the child
// directory nor the destination. Those are what the code below is about.
// ---------------------------------------------------------------------------

/// The per-project directory the coordinator context is published in.
pub const CONTEXT_DIR_NAME: &str = ".dot-agent-deck";

/// The file inside it. Matched by `read_back_task` and by every agent-facing
/// instruction `build_orchestrator_context` emits.
pub const CONTEXT_FILE_NAME: &str = "orchestrator-context.md";

/// Upper bound on a composed coordinator context this process will write.
///
/// **4 MiB.** The task is already bounded at the wire boundary
/// ([`crate::bounded_read::MAX_TASK_BYTES`], 1 MiB) but the *composed* output is
/// task + template + role names + descriptions, and everything after the task
/// comes out of a config file — bounded at
/// [`crate::project_resolve::MAX_PROJECT_CONFIG_BYTES`] (1 MiB) for a
/// caller-selected path, and bounded by nothing at all on the interactive
/// `Ctrl+n` path, which loads through `project_config::load_project_config`.
/// So the two input bounds imply a ~2 MiB ceiling for the daemon verb, and 4 MiB
/// is twice that: no legitimate maximal input can be refused, and the write is
/// still capped at a quarter of the protocol's own `MAX_FRAME_LEN`.
///
/// Refused rather than truncated, for [`crate::bounded_read::read_capped`]'s
/// reason: a silently shortened coordinator context is a wrong brief that looks
/// like a right one, and the agent acting on it has no way to tell.
///
/// **Be honest about which caller this actually stops.** For the daemon verb it
/// is a backstop rather than the operative gate — the two input bounds already
/// imply a smaller ceiling, so a request that passes them cannot reach this one.
/// It becomes load-bearing in two cases: the in-process paths, which compose
/// from a config read by the *unbounded* loader, and any later widening of
/// either input bound. A bound whose only justification is "the callers happen
/// to be smaller today" is a bound that disappears the first time one of them
/// grows, which is why it is checked here — at the write — rather than inferred
/// at the boundary.
pub const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;

/// Why a coordinator context was not published.
///
/// Replaces the `Option` the old publish returned. The caller needs to know
/// *why* — the daemon has to answer a client, and the daemon log needs the
/// detail — and "it did not work" is not an answer either can act on.
///
/// Two renderings, deliberately: [`Display`](std::fmt::Display) is the
/// daemon-local diagnostic, and [`ContextPublishError::client_sentence`] is what
/// may cross the wire. Neither names a path today; the split exists so the
/// daemon-local one can grow an OS error string without that decision leaking
/// onto the wire by default.
#[derive(Debug)]
pub enum ContextPublishError {
    /// The composed context exceeds [`MAX_CONTEXT_BYTES`].
    ContextTooLarge(usize),
    /// The final `.dot-agent-deck` component is a symlink. Refused; see
    /// [`open_context_dir`].
    ContextDirIsSymlink,
    /// `.dot-agent-deck` could not be created or opened as a directory — it is a
    /// regular file, the project directory does not exist, or the permissions
    /// forbid it.
    ContextDirUnusable(std::io::Error),
    /// The `.dot-agent-deck` component was replaced between the moment it was
    /// opened and the moment the temp file was created inside it. See
    /// [`publish_orchestrator_context`] for what this detects and what it does
    /// not prevent.
    ContextDirReplaced,
    /// The owner-only temp file could not be created.
    TempCreate(std::io::Error),
    /// The bytes could not be written to the temp file.
    TempWrite(std::io::Error),
    /// The rename that publishes the temp file over the destination failed.
    Publish(std::io::Error),
}

impl ContextPublishError {
    /// The **daemon-local** diagnostic. Safe to log; carries the OS error.
    pub fn detail(&self) -> String {
        match self {
            Self::ContextTooLarge(n) => format!(
                "the composed coordinator context is {n} bytes; at most {MAX_CONTEXT_BYTES} can \
                 be published"
            ),
            Self::ContextDirIsSymlink => format!(
                "{CONTEXT_DIR_NAME} is a symlink; the coordinator context must be published into \
                 a real directory in the project itself"
            ),
            Self::ContextDirUnusable(e) => {
                format!("{CONTEXT_DIR_NAME} could not be created or opened as a directory: {e}")
            }
            Self::ContextDirReplaced => format!(
                "{CONTEXT_DIR_NAME} was replaced while the coordinator context was being written"
            ),
            Self::TempCreate(e) => format!("could not create the temporary context file: {e}"),
            Self::TempWrite(e) => format!("could not write the temporary context file: {e}"),
            Self::Publish(e) => format!("could not publish the coordinator context: {e}"),
        }
    }

    /// The sentence that may cross the wire.
    ///
    /// It names no path and carries no raw OS error, which is the same rule
    /// [`crate::project_resolve::generic_refusal`] follows. It is deliberately
    /// **not** the single uniform sentence that one is, though, and the reason
    /// is that the two protect different things: a resolve refusal must not
    /// distinguish "no such directory" from "no config there" for an arbitrary
    /// pasted path, whereas every case here is reached only *after* that path
    /// has already resolved as a project — so the caller has already learned the
    /// directory exists and holds a config, and naming the shape of the
    /// obstruction discloses nothing further while being the only way the
    /// operator can fix it.
    pub fn client_sentence(&self) -> &'static str {
        match self {
            Self::ContextTooLarge(_) => {
                "the composed coordinator context exceeds this daemon's size limit"
            }
            Self::ContextDirIsSymlink => {
                "the project's `.dot-agent-deck` directory is a symlink, which is refused"
            }
            Self::ContextDirUnusable(_) => {
                "the project's `.dot-agent-deck` directory could not be created"
            }
            Self::ContextDirReplaced => {
                "the project's `.dot-agent-deck` directory changed while the context was being \
                 written"
            }
            Self::TempCreate(_) | Self::TempWrite(_) | Self::Publish(_) => {
                "the coordinator context could not be written"
            }
        }
    }
}

impl std::fmt::Display for ContextPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

/// What [`open_context_dir`] hands back so the publish can prove, after the
/// fact, that it wrote into the directory it checked.
///
/// A real open handle on Unix; a marker on other platforms, where a directory
/// cannot be opened as a `File` without platform-specific flags and the
/// symlink check is a separate lookup anyway.
#[cfg(unix)]
type ContextDirGuard = std::fs::File;
#[cfg(not(unix))]
#[derive(Debug)]
struct ContextDirGuard;

/// Create `<project>/.dot-agent-deck` **owner-only** if it is not there.
///
/// `DirBuilder::mode(0o700)` rather than a `chmod` afterwards: the mode is
/// applied by `mkdir(2)` itself, so there is no window in which the directory
/// exists group- or world-readable. A permissive umask cannot widen it either —
/// a umask only *removes* bits, so the result is `0o700 & !umask`, which is
/// owner-only or narrower whatever the caller's umask is.
///
/// **A directory that already exists is left exactly as it is.** Publishing is
/// not the operation that gets to re-permission a directory the operator
/// created, and every `.dot-agent-deck` in every existing checkout predates
/// this rule. So the owner-only claim is about directories this function
/// *creates*, and no wider.
///
/// Non-recursive on purpose: the project directory is the caller's to establish
/// (the daemon verb canonicalises it first, which proves it exists), and
/// `create_dir_all` would silently invent an entire chain for a typo.
fn create_context_dir(dir: &std::path::Path) -> Result<(), ContextPublishError> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(ContextPublishError::ContextDirUnusable(e)),
    }
}

/// Open `.dot-agent-deck`, refusing a symlinked final component.
///
/// On Unix the open carries `O_NOFOLLOW | O_DIRECTORY`, following
/// [`crate::project_resolve::read_config_file`]'s precedent: the refusal is a
/// property of the `open(2)` itself rather than of a check-then-open pair, and
/// `O_DIRECTORY` additionally refuses a `.dot-agent-deck` that is a regular
/// file. The handle is kept so the publish can compare it against the path
/// afterwards.
///
/// **On a platform without those flags the guarantee is narrower**, and is
/// stated rather than papered over: the check is a separate `symlink_metadata`
/// lookup from the write, so a component swapped between the two is not caught,
/// and no mode bits are applied at all (the Windows protected-DACL equivalent
/// is not implemented). PRD #819's threat model is a daemon reachable off-box,
/// and that daemon is Unix-only today — `bind_attach_listener` is a
/// Unix-domain socket and the whole L2 tier is `#![cfg(unix)]`.
fn open_context_dir(dir: &std::path::Path) -> Result<ContextDirGuard, ContextPublishError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(dir)
            .map_err(|e| {
                // Consulting the path here cannot reintroduce a TOCTOU the open
                // handle exists to avoid: the open has ALREADY failed, so
                // nothing is written on this branch either way, and the only
                // thing a race can change is the wording of an error returned
                // regardless. Same recovery, for the same reason, as
                // `project_resolve::read_config_file`.
                if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
                    ContextPublishError::ContextDirIsSymlink
                } else {
                    ContextPublishError::ContextDirUnusable(e)
                }
            })
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ContextPublishError::ContextDirIsSymlink);
        }
        if !std::fs::metadata(dir).is_ok_and(|m| m.is_dir()) {
            return Err(ContextPublishError::ContextDirUnusable(
                std::io::Error::other(format!("{CONTEXT_DIR_NAME} is not a directory")),
            ));
        }
        Ok(ContextDirGuard)
    }
}

/// Whether the directory `guard` was opened on is still the one at `dir`.
///
/// Compares device + inode from the **open handle's** `fstat` against a
/// `symlink_metadata` of the path. This **detects** a `.dot-agent-deck`
/// swapped between [`open_context_dir`] and the temp-file create; it does not
/// **prevent** one. Preventing it needs `openat(2)` from the held descriptor,
/// which `std` does not expose and which is not worth hand-rolling here: the
/// swap requires write permission on the project directory, and anyone holding
/// that can rewrite `.dot-agent-deck.toml` — whose `command` strings the daemon
/// executes — which is strictly more authority than redirecting one markdown
/// file. The check is cheap, so it is here; the claim is exactly that.
#[cfg(unix)]
fn context_dir_unchanged(guard: &ContextDirGuard, dir: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (guard.metadata(), std::fs::symlink_metadata(dir)) {
        (Ok(open), Ok(now)) => open.dev() == now.dev() && open.ino() == now.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn context_dir_unchanged(_guard: &ContextDirGuard, _dir: &std::path::Path) -> bool {
    // No handle to compare against; see `open_context_dir`'s narrower guarantee.
    true
}

/// A temp-file name unique within one directory, for one publish.
///
/// Process id plus a monotonically increasing counter: two publishes in one
/// process cannot collide, and two processes cannot either. It is only ever
/// half of the guarantee — the create is `create_new`, so a collision fails
/// loudly rather than clobbering — and it is hidden and suffixed so it can never
/// be mistaken for a coordinator context by [`read_back_task`], which reads
/// exactly [`CONTEXT_FILE_NAME`].
fn temp_context_file_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        ".{CONTEXT_FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Publish `content` at `<project_dir>/.dot-agent-deck/orchestrator-context.md`,
/// atomically and owner-only, and answer with the path written.
///
/// Five properties, each of which the `create_dir_all` + `std::fs::write` pair
/// it replaced lacked:
///
/// * **Bounded.** The composed context is checked against
///   [`MAX_CONTEXT_BYTES`] before a filesystem is touched, and refused rather
///   than truncated.
/// * **The directory component is not a symlink.** [`open_context_dir`] refuses
///   one at the `open(2)`, and [`context_dir_unchanged`] then detects a swap
///   afterwards.
/// * **Owner-only from creation, never by a later `chmod`.** The directory is
///   created `0o700` by `mkdir(2)` and the file `0o600` by `open(2)`, so there
///   is no window in which either exists wider than that. A permissive umask
///   only removes bits and a permissive parent directory grants nothing here,
///   because neither is consulted for the new inode's mode.
/// * **Atomic with respect to a reader.** The bytes go to a `create_new`
///   temp file in the SAME directory and reach the destination by `rename(2)`,
///   so a concurrent reader sees either the previous context or the new one and
///   never a prefix of the new one. This is atomicity, **not durability**: no
///   `fsync` is issued, so a machine that loses power immediately afterwards may
///   come back to either version. Publishing a coordinator context is
///   worth-redoing work, not a ledger.
/// * **A destination symlink is replaced, not followed.** `rename(2)` operates
///   on the directory entry, so a `orchestrator-context.md` that is a symlink to
///   `/etc/passwd` is *unlinked* and replaced by the new regular file; nothing is
///   written through it. This is the one property that comes free from choosing
///   rename over write, and it is the reason the choice is not merely about
///   atomicity.
///
/// A failure leaves the previous context — if any — exactly as it was, and
/// removes the temp file. That, not the absence of a partially written
/// destination alone, is what "a partial write must never be observable as a
/// coordinator context" means.
///
/// **Blocking.** Async callers go through [`crate::project_resolve::run_bounded`].
pub fn publish_orchestrator_context(
    project_dir: &std::path::Path,
    content: &str,
) -> Result<std::path::PathBuf, ContextPublishError> {
    if content.len() > MAX_CONTEXT_BYTES {
        return Err(ContextPublishError::ContextTooLarge(content.len()));
    }

    let dir = project_dir.join(CONTEXT_DIR_NAME);
    create_context_dir(&dir)?;
    let guard = open_context_dir(&dir)?;

    let final_path = dir.join(CONTEXT_FILE_NAME);
    let temp_path = dir.join(temp_context_file_name());

    let outcome = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // The mode is an argument to `open(2)`, so the file is 0600 from the
            // instant it exists. `O_NOFOLLOW` costs nothing next to
            // `create_new` and states the intent at the same seam the directory
            // open states it.
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(ContextPublishError::TempCreate)?;

        if !context_dir_unchanged(&guard, &dir) {
            return Err(ContextPublishError::ContextDirReplaced);
        }

        use std::io::Write as _;
        file.write_all(content.as_bytes())
            .map_err(ContextPublishError::TempWrite)?;
        file.flush().map_err(ContextPublishError::TempWrite)?;
        drop(file);

        std::fs::rename(&temp_path, &final_path).map_err(ContextPublishError::Publish)
    })();

    match outcome {
        Ok(()) => Ok(final_path),
        Err(e) => {
            // Best effort, and deliberately not reported: the publish already
            // failed for a reason the caller is about to be told, and a leftover
            // temp file is not that reason. `TempCreate` is the one case where
            // there is nothing to remove, and removing a path that is not there
            // is a no-op.
            let _ = std::fs::remove_file(&temp_path);
            Err(e)
        }
    }
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
