use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::project_config::CONFIG_FILE_NAME;

const TEMPLATE: &str = r#"# dot-agent-deck project configuration
# Defines an orchestration: a team of agents in one tab, where the start role
# (the orchestrator) delegates work to the others with `dot-agent-deck delegate`.
# Open it from the New Agent form (Ctrl+N) by picking "Orch: team" on the Mode
# row. Run `dot-agent-deck validate` after editing.

[[orchestrations]]
name = "team"
# default = true    # the orchestration a bare `dispatch` / scheduled run opens

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
# agent = "claude"  # declare the agent when `command` is a launcher script
prompt_template = """
You coordinate the team. Break the user's request into tasks and delegate each
one to the worker with `dot-agent-deck delegate`; do not implement it yourself.
"""

[[orchestrations.roles]]
name = "worker"
command = "claude"
description = "Implements the task it is given and reports back when done"
# clear = true      # start each delegated task from a fresh session (default)
"#;

pub fn run_init(path: &Path) -> ExitCode {
    let file_path = path.join(CONFIG_FILE_NAME);

    let mut file = match File::create_new(&file_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("{} already exists", file_path.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("Failed to create {}: {e}", file_path.display());
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = file.write_all(TEMPLATE.as_bytes()) {
        eprintln!("Failed to write {}: {e}", file_path.display());
        return ExitCode::FAILURE;
    }

    println!("Created {}", file_path.display());

    // Issue #329 §2: the ignore rule the generated advice needs, in the one
    // place that is per-clone and never committed. Best-effort and reported
    // rather than fatal — `init` succeeded at what it was asked to do, and a
    // project that is not a git repository has nothing to exclude from.
    match crate::orchestrator_context::ensure_git_excludes_context_dir(path) {
        Ok(crate::orchestrator_context::GitExcludeOutcome::Added) => {
            println!("Excluded .dot-agent-deck/ in .git/info/exclude");
        }
        Ok(_) => {}
        Err(e) => eprintln!("Could not update .git/info/exclude: {e}"),
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_validation::validate_config;
    use crate::project_config::ProjectConfig;

    /// Issue #1199: the starter `init` writes is an orchestration (workspace
    /// modes were removed), and it is a config `dot-agent-deck validate` has
    /// nothing to say about — not even a warning.
    #[test]
    fn template_parses_and_validates_cleanly() {
        let config: ProjectConfig = toml::from_str(TEMPLATE).expect("the init template parses");
        assert!(!config.legacy_modes_declared);
        assert_eq!(config.orchestrations.len(), 1);
        let issues = validate_config(&config);
        assert!(
            issues.is_empty(),
            "the init template must validate with no issues; got {issues:?}"
        );
    }
}
