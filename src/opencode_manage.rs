use std::path::PathBuf;

fn plugin_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".opencode")
        .join("plugin")
        .join("dot-agent-deck")
}

fn plugin_template(binary_path: &str) -> String {
    format!(
        r#"const {{ execSync }} = require("child_process");

module.exports = {{
  name: "dot-agent-deck",
  subscribe: [
    "session.created",
    "session.deleted",
    "session.idle",
    "session.error",
    "session.status.updated",
    "tool.execute.before",
    "tool.execute.after",
    "permission.asked",
  ],
  onEvent(event) {{
    try {{
      execSync("{binary_path} hook --agent opencode", {{
        input: JSON.stringify(event),
        timeout: 5000,
        stdio: ["pipe", "ignore", "ignore"],
      }});
    }} catch (_) {{}}
  }},
}};
"#
    )
}

fn install_impl(dir: &PathBuf, binary_path: &str) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("Error creating plugin directory {}: {e}", dir.display());
        return;
    }

    let path = dir.join("index.js");
    let content = plugin_template(binary_path);
    if let Err(e) = std::fs::write(&path, content) {
        eprintln!("Error writing plugin {}: {e}", path.display());
        return;
    }

    println!("Installed OpenCode plugin: {}", path.display());
}

fn uninstall_impl(dir: &PathBuf) {
    if !dir.exists() {
        println!("No OpenCode plugin found to remove.");
        return;
    }

    if let Err(e) = std::fs::remove_dir_all(dir) {
        eprintln!("Error removing plugin directory {}: {e}", dir.display());
        return;
    }

    println!("Removed OpenCode plugin: {}", dir.display());
}

pub fn install() {
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    install_impl(&plugin_dir(), &binary_path);
}

pub fn uninstall() {
    uninstall_impl(&plugin_dir());
}

// --- Testable versions that accept a custom path ---

pub fn install_to(dir: &PathBuf, binary_path: &str) {
    install_impl(dir, binary_path);
}

pub fn uninstall_from(dir: &PathBuf) {
    uninstall_impl(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_template_contains_binary_path() {
        let content = plugin_template("/usr/local/bin/dot-agent-deck");
        assert!(content.contains("/usr/local/bin/dot-agent-deck hook --agent opencode"));
        assert!(content.contains("dot-agent-deck"));
        assert!(content.contains("session.created"));
        assert!(content.contains("onEvent"));
    }

    #[test]
    fn install_creates_plugin_file() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck");

        let index = plugin_dir.join("index.js");
        assert!(index.exists());
        let content = std::fs::read_to_string(&index).unwrap();
        assert!(content.contains("/usr/local/bin/dot-agent-deck hook --agent opencode"));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck");
        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck");

        let index = plugin_dir.join("index.js");
        assert!(index.exists());
        let content = std::fs::read_to_string(&index).unwrap();
        assert!(content.contains("dot-agent-deck"));
    }

    #[test]
    fn uninstall_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck");
        assert!(plugin_dir.exists());

        uninstall_from(&plugin_dir);
        assert!(!plugin_dir.exists());
    }

    #[test]
    fn uninstall_noop_when_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("nonexistent");

        uninstall_from(&plugin_dir); // Should not panic
    }
}
