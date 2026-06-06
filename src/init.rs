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
