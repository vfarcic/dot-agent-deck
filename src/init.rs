use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use crate::project_config::CONFIG_FILE_NAME;

const TEMPLATE: &str = r#"# dot-agent-deck project configuration
# Defines workspace modes for the dot-agent-deck dashboard.
# Each [[modes]] block creates a named layout with persistent panes
# and reactive command-routing rules.

[[modes]]
name = "dev"
# shell_init = "devbox shell"    # Optional: initialize the shell environment

# Persistent panes run continuously alongside your agent session.

[[modes.panes]]
command = "git log --oneline -20"
name = "Recent Commits"

# [[modes.panes]]
# command = "cargo watch -x check"
# name = "Compiler"

# Rules route agent commands matching a regex to reactive side panes.
#   pattern  — regex matched against commands the agent executes
#   watch    — if true, re-run the command on an interval (default: false)
#   interval — refresh interval in seconds (only when watch = true)

[[modes.rules]]
pattern = "cargo\\s+(build|test|check)"
watch = false

# [[modes.rules]]
# pattern = "kubectl\\s+get"
# watch = true
# interval = 5
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
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitCode;
    use tempfile::tempdir;

    #[test]
    fn creates_config_file() {
        let dir = tempdir().unwrap();
        let result = run_init(dir.path());
        assert_eq!(result, ExitCode::SUCCESS);

        let content = std::fs::read_to_string(dir.path().join(CONFIG_FILE_NAME)).unwrap();
        assert!(content.contains("[[modes]]"));
        assert!(content.contains("[[modes.panes]]"));
        assert!(content.contains("[[modes.rules]]"));
    }

    #[test]
    fn does_not_overwrite_existing() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join(CONFIG_FILE_NAME);
        std::fs::write(&file_path, "original").unwrap();

        let result = run_init(dir.path());
        assert_eq!(result, ExitCode::FAILURE);

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "original");
    }

    #[test]
    fn fails_on_invalid_path() {
        let result = run_init(Path::new("/nonexistent/directory/path"));
        assert_eq!(result, ExitCode::FAILURE);
    }
}
