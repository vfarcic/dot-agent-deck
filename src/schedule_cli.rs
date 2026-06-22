//! The single validated writer for the global `schedules.toml` (PRD #127 M1.5).
//!
//! All three edit doors (agent→CLI, CLI directly, hand-edit) funnel mutations
//! through these helpers so an LLM or a script can't silently produce a
//! malformed cron or an unescaped prompt. The writer:
//!
//! - validates the cron via [`crate::scheduler::validate_cron`],
//! - expands `~`/`$VAR` in `working_dir` via [`crate::config::expand_path`],
//! - **forbids rename** (there is no new-name field; `update` keys by `name`
//!   and errors on an unknown name) because `name` is the reuse-registry key,
//! - writes **atomically** (temp file + rename) to the fixed global path from
//!   [`crate::config::schedules_path`], **regardless of cwd**.
//!
//! The daemon-reload trigger after a mutating command lives in `main.rs` (it
//! needs the async socket client); everything here is pure data so it is unit
//! testable without a daemon.

use std::path::Path;

use serde::Serialize;

use crate::config::{ScheduledTask, expand_path};
use crate::scheduler::validate_cron;

/// Parsed fields for `schedule add`.
pub struct AddArgs {
    pub name: String,
    pub cron: String,
    pub working_dir: String,
    pub command: Option<String>,
    pub prompt: String,
    pub new_tab_per_fire: bool,
    pub enabled: bool,
}

/// Parsed fields for `schedule update`. Every field except `name` is optional;
/// `None` means "leave unchanged". There is deliberately **no** name-change
/// field — rename is forbidden (see module docs).
#[derive(Default)]
pub struct UpdateArgs {
    pub name: String,
    pub cron: Option<String>,
    pub working_dir: Option<String>,
    pub command: Option<String>,
    pub prompt: Option<String>,
    pub new_tab_per_fire: Option<bool>,
    pub enabled: Option<bool>,
}

/// Append a new task. Errors if the cron is malformed or a task with the same
/// name already exists (use `update` to change one).
pub fn add(tasks: &mut Vec<ScheduledTask>, args: AddArgs) -> Result<(), String> {
    // PRD #127 follow-up (USER DECISION): a scheduled task's `command` is now
    // REQUIRED — there is no silent `$SHELL` fallback, because a bare shell
    // can't act on the scheduled prompt. `schedule update` deliberately does
    // NOT re-require it (a stored task already has one).
    match &args.command {
        Some(cmd) if !cmd.trim().is_empty() => {}
        _ => {
            return Err(
                "--command is required: a scheduled task needs an agent command \
                 (e.g. claude) to act on its prompt"
                    .to_string(),
            );
        }
    }
    validate_cron(&args.cron).map_err(|e| format!("invalid cron expression: {e}"))?;
    if tasks.iter().any(|t| t.name == args.name) {
        return Err(format!(
            "a schedule named {:?} already exists; use `schedule update` to change it",
            args.name
        ));
    }
    tasks.push(ScheduledTask {
        name: args.name,
        cron: args.cron,
        working_dir: expand_path(&args.working_dir),
        command: args.command,
        prompt: args.prompt,
        new_tab_per_fire: args.new_tab_per_fire,
        enabled: args.enabled,
        // PRD #120 issue-dispatch tasks are authored by a separate door; the
        // #127 `schedule add` CLI only writes single-spawn tasks.
        issue_dispatch: None,
    });
    Ok(())
}

/// Apply an in-place update to the task named `args.name`. Errors if no such
/// task exists (the rename guard: you cannot move a definition to a new name)
/// or if a supplied cron is malformed. The task's `name` is never touched.
pub fn update(tasks: &mut [ScheduledTask], args: UpdateArgs) -> Result<(), String> {
    if let Some(cron) = &args.cron {
        validate_cron(cron).map_err(|e| format!("invalid cron expression: {e}"))?;
    }
    let task = tasks
        .iter_mut()
        .find(|t| t.name == args.name)
        .ok_or_else(|| format!("no schedule named {:?}", args.name))?;
    if let Some(cron) = args.cron {
        task.cron = cron;
    }
    if let Some(wd) = args.working_dir {
        task.working_dir = expand_path(&wd);
    }
    if let Some(cmd) = args.command {
        task.command = Some(cmd);
    }
    if let Some(prompt) = args.prompt {
        task.prompt = prompt;
    }
    if let Some(ntpf) = args.new_tab_per_fire {
        task.new_tab_per_fire = ntpf;
    }
    if let Some(enabled) = args.enabled {
        task.enabled = enabled;
    }
    Ok(())
}

/// Remove the task named `name`. Errors if it does not exist.
pub fn remove(tasks: &mut Vec<ScheduledTask>, name: &str) -> Result<(), String> {
    let before = tasks.len();
    tasks.retain(|t| t.name != name);
    if tasks.len() == before {
        return Err(format!("no schedule named {name:?}"));
    }
    Ok(())
}

/// Set the `enabled` flag on the task named `name`. Errors if it does not
/// exist. Backs `schedule enable` / `schedule disable`.
pub fn set_enabled(tasks: &mut [ScheduledTask], name: &str, enabled: bool) -> Result<(), String> {
    let task = tasks
        .iter_mut()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("no schedule named {name:?}"))?;
    task.enabled = enabled;
    Ok(())
}

#[derive(Serialize)]
struct SchedulesDoc<'a> {
    #[serde(rename = "scheduled_tasks")]
    scheduled_tasks: &'a [ScheduledTask],
}

/// Serialize `tasks` and write them to `path` atomically: write to a temp file
/// in the **same directory** as `path`, then rename over `path`. Rename within
/// one directory is atomic on POSIX, so a concurrent reader (the daemon's
/// reload) never sees a half-written file. The temp file lives next to the
/// target — never under the process cwd — so the writer targets the fixed
/// global path regardless of where the CLI was invoked.
pub fn write_atomic(path: &Path, tasks: &[ScheduledTask]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("schedules path {} has no parent directory", path.display()))?;
    // PRD #127 S2: create the config dir 0700 (owner-only). `DirBuilder`'s mode
    // applies only to directories it newly creates, so an existing shared dir
    // keeps its mode — we don't surprise-tighten a dir we didn't make.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| {
            format!(
                "failed to create config directory {}: {e}",
                parent.display()
            )
        })?;

    let doc = SchedulesDoc {
        scheduled_tasks: tasks,
    };
    let contents =
        toml::to_string_pretty(&doc).map_err(|e| format!("failed to serialize schedules: {e}"))?;

    // PRD #127 S1/S2: prompts may carry secrets. Create the temp file with
    // `O_EXCL` (`create_new`, no symlink-following onto an attacker-planted
    // target) at mode 0600, write, then atomically rename over the final path —
    // the rename preserves the 0600 inode, matching the daemon socket's trust
    // boundary. A unique temp name (pid + monotonic counter) avoids colliding
    // with a concurrent writer or a stale leftover.
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".schedules.toml.tmp.{}.{unique}",
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| {
            format!(
                "failed to create temp schedules file {}: {e}",
                tmp.display()
            )
        })?;
    use std::io::Write as _;
    let write_result = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "failed to write temp schedules file {}: {e}",
            tmp.display()
        ));
    }
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!(
            "failed to rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Render the schedule list for `schedule list`: one line per task with its
/// enabled/disabled state and next-fire time (local). A malformed cron (should
/// not happen for a writer-produced file) renders as `next: <invalid cron>`.
pub fn format_list(tasks: &[ScheduledTask]) -> String {
    if tasks.is_empty() {
        return "No scheduled tasks.".to_string();
    }
    use chrono::Local;
    let mut out = String::new();
    for t in tasks {
        let state = if t.enabled { "enabled " } else { "disabled" };
        let next = match crate::scheduler::parse_cron(&t.cron) {
            Ok(schedule) => match schedule.upcoming(Local).next() {
                Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
                None => "none".to_string(),
            },
            Err(_) => "<invalid cron>".to_string(),
        };
        out.push_str(&format!(
            "{state}  {name}  cron={cron:?}  next={next}  dir={dir}\n",
            name = t.name,
            cron = t.cron,
            dir = t.working_dir,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_add(name: &str, cron: &str) -> AddArgs {
        AddArgs {
            name: name.to_string(),
            cron: cron.to_string(),
            working_dir: "/tmp".to_string(),
            // `command` is now required (PRD #127 follow-up); the helper supplies
            // one so the other cases exercise the field they care about.
            command: Some("claude".to_string()),
            prompt: "hi".to_string(),
            new_tab_per_fire: false,
            enabled: true,
        }
    }

    // PRD #127 follow-up — `add` REQUIRES a non-empty `command`. A missing or
    // blank command is rejected with an error that names `--command` and says it
    // is required (so the CLI surfaces the contract), and no task is appended.
    #[test]
    fn add_requires_non_empty_command() {
        for bad in [None, Some(String::new()), Some("   ".to_string())] {
            let mut tasks = Vec::new();
            let mut args = sample_add("needs-command", "0 9 * * *");
            args.command = bad.clone();
            let err = add(&mut tasks, args).unwrap_err();
            let lowered = err.to_lowercase();
            assert!(
                lowered.contains("command") && lowered.contains("required"),
                "error must name --command as required, got: {err} (for {bad:?})"
            );
            assert!(
                tasks.is_empty(),
                "no task should be added when command is missing/blank"
            );
        }
    }

    // scheduler/cli/001 (validation) — add rejects a malformed cron expression
    // with a nonzero-path Err rather than writing a broken entry.
    #[test]
    fn add_rejects_malformed_cron() {
        let mut tasks = Vec::new();
        let err = add(&mut tasks, sample_add("bad", "not a cron")).unwrap_err();
        assert!(err.contains("invalid cron"), "got: {err}");
        assert!(
            tasks.is_empty(),
            "no task should be added on validation failure"
        );

        // A valid 5-field POSIX cron is accepted.
        assert!(add(&mut tasks, sample_add("good", "0 9 * * MON-FRI")).is_ok());
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn add_rejects_duplicate_name() {
        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("dup", "0 9 * * *")).unwrap();
        let err = add(&mut tasks, sample_add("dup", "0 9 * * *")).unwrap_err();
        assert!(err.contains("already exists"), "got: {err}");
    }

    // scheduler/cli/001 (rename rejection) — `update` keys by name and there is
    // no name-change field, so a definition can never be renamed: updating an
    // unknown name errors, and updating a known one leaves `name` untouched.
    #[test]
    fn update_forbids_rename_and_errors_on_unknown() {
        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("keep", "0 9 * * *")).unwrap();

        // Unknown name → error (cannot "rename" by updating a fresh name).
        let err = update(
            &mut tasks,
            UpdateArgs {
                name: "other".to_string(),
                cron: Some("0 10 * * *".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("no schedule named"), "got: {err}");

        // Updating an existing task changes fields but never the name.
        update(
            &mut tasks,
            UpdateArgs {
                name: "keep".to_string(),
                cron: Some("0 11 * * *".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "keep");
        assert_eq!(tasks[0].cron, "0 11 * * *");
    }

    #[test]
    fn update_rejects_malformed_cron() {
        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("t", "0 9 * * *")).unwrap();
        let err = update(
            &mut tasks,
            UpdateArgs {
                name: "t".to_string(),
                cron: Some("99 99 * * *".to_string()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.contains("invalid cron"), "got: {err}");
        // The original cron is untouched.
        assert_eq!(tasks[0].cron, "0 9 * * *");
    }

    #[test]
    fn remove_and_set_enabled_error_on_unknown() {
        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("t", "0 9 * * *")).unwrap();
        assert!(remove(&mut tasks, "nope").is_err());
        assert!(set_enabled(&mut tasks, "nope", false).is_err());
        set_enabled(&mut tasks, "t", false).unwrap();
        assert!(!tasks[0].enabled);
        remove(&mut tasks, "t").unwrap();
        assert!(tasks.is_empty());
    }

    // scheduler/cli/001 (atomic write regardless of cwd) — `write_atomic`
    // targets the absolute path it is given and drops its temp file next to
    // that path, never under the process cwd.
    #[test]
    fn write_atomic_targets_absolute_path_not_cwd() {
        // Serialize cwd mutation against any other test that fiddles with it.
        static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let target_dir = tempfile::tempdir().unwrap();
        let cwd_dir = tempfile::tempdir().unwrap();
        let target = target_dir.path().join("nested/schedules.toml");

        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd_dir.path()).unwrap();

        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("cli-task", "*/5 * * * * *")).unwrap();
        let result = write_atomic(&target, &tasks);

        // Restore cwd before asserting so a failure doesn't strand the suite.
        std::env::set_current_dir(&prev_cwd).unwrap();
        result.unwrap();

        // (a) The file landed at the absolute target (parent auto-created).
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(written.contains("cli-task"), "got:\n{written}");
        // It round-trips back through the loader.
        let reloaded = crate::config::LoadedSchedules::parse(&written);
        assert!(reloaded.errors.is_empty());
        assert_eq!(reloaded.tasks.len(), 1);

        // (b) Nothing was written under the cwd, and no temp file leaked.
        assert!(
            !cwd_dir.path().join("schedules.toml").exists(),
            "writer must not drop a file under the cwd"
        );
        let leaked: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leaked.is_empty(), "temp file should have been renamed away");
    }

    // PRD #127 S1/S2 — schedules.toml (which may carry secret prompts) is
    // written owner-only (0600), and the temp it's renamed from is also 0600.
    #[test]
    fn write_atomic_writes_owner_only_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/schedules.toml");
        let mut tasks = Vec::new();
        add(&mut tasks, sample_add("secretful", "0 9 * * *")).unwrap();
        write_atomic(&target, &tasks).unwrap();

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "schedules.toml must be owner-only (0600), got {:o}",
            mode & 0o777
        );
        // The config dir we created is owner-only too.
        let dir_mode = std::fs::metadata(target.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "config dir should be 0700");
    }
}
