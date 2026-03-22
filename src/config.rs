use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
    }

    PathBuf::from("/tmp/dot-agent-deck.sock")
}
