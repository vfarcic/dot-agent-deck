use std::path::PathBuf;

use serde::Deserialize;

pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
    }

    PathBuf::from("/tmp/dot-agent-deck.sock")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    pub default_command: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            default_command: String::new(),
        }
    }
}

impl DashboardConfig {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

fn config_path() -> PathBuf {
    if let Ok(dir) = std::env::var("DOT_AGENT_DECK_CONFIG") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".config/dot-agent-deck/config.toml")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}
