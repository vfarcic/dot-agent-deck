use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::hook_binary::HookBinaryNotice;

pub struct StaleHome {
    pub home: PathBuf,
    pub pin: PathBuf,
    _dir: tempfile::TempDir,
}

impl StaleHome {
    pub fn new() -> Self {
        let dir = crate::common::race_safe_tempdir();
        let home = dir.path().join("home");
        let pin = dir.path().join("opt/old/dot-agent-deck");
        std::fs::create_dir_all(pin.parent().unwrap()).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(&pin, "#!/bin/sh\nprintf 'dot-agent-deck 0.0.1\\n'\n").unwrap();
        std::fs::set_permissions(&pin, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Overriding the harness HOME also replaces its installed-binary link.
        // Supply one here so startup reads the old pin even on a machine with
        // no deck on PATH. The target remains a cargo artifact, so this link
        // resolves the installer without making the running build takeover-eligible.
        let bin_dir = home.join(".local/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_dot-agent-deck"),
            bin_dir.join("dot-agent-deck"),
        )
        .unwrap();
        let fixture = Self {
            home,
            pin,
            _dir: dir,
        };
        fixture.seed_hooks();
        fixture
    }

    pub fn seed_hooks(&self) {
        let path = self.home.join(".claude/settings.json");
        let mut settings: serde_json::Value = std::fs::read_to_string(&path)
            .map(|text| serde_json::from_str(&text).unwrap())
            .unwrap_or_else(|_| serde_json::json!({}));
        settings["hooks"] = serde_json::json!({"PreToolUse": [{"hooks": [{
            "type": "command", "command": format!("'{}' hook --agent claude-code", self.pin.display())
        }]}]});
        std::fs::write(path, settings.to_string()).unwrap();
    }
}

pub fn notices(socket: &Path) -> Vec<HookBinaryNotice> {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        DaemonClient::new(socket.to_path_buf())
            .hook_binary_notices()
            .await
            .expect("Hello exchange")
            .expect("Hello includes hook_binary_notices")
    })
}
