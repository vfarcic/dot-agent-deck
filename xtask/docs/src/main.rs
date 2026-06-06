//! `cargo xtask docs --tests` — generate one Markdown file per
//! `#[spec]`-annotated test. PRD #77 Decision 30 + M4.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Accept and ignore a leading `--tests` (the documented form is
    // `cargo xtask docs --tests`); the binary works the same with
    // or without it. Anything else surfaces a usage line so a
    // future flag isn't silently consumed.
    for arg in &args {
        match arg.as_str() {
            "--tests" => {}
            "-h" | "--help" => {
                println!("usage: cargo xtask docs --tests");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask docs: unknown argument {other:?}");
                eprintln!("usage: cargo xtask docs --tests");
                return ExitCode::from(2);
            }
        }
    }

    let workspace_root = match find_workspace_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::from(2);
        }
    };
    let config = xtask_docs::DocsConfig::from_workspace(workspace_root);
    let generated = match xtask_docs::generate_all(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let written = match xtask_docs::write_all(&generated) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    for path in &written {
        // Print the workspace-relative form so the output is easy
        // to copy-paste into a follow-up command.
        let rel = path
            .strip_prefix(&config.workspace_root)
            .unwrap_or(path.as_path());
        println!("wrote {}", rel.display());
    }
    ExitCode::SUCCESS
}

fn find_workspace_root() -> Result<PathBuf, String> {
    let mut dir = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if let Ok(s) = std::fs::read_to_string(&candidate)
            && s.contains("[workspace]")
        {
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(format!(
                "could not locate workspace root from {}",
                std::env::current_dir().unwrap_or_default().display()
            ));
        }
    }
}
