#![cfg(unix)]
//! Filesystem-only S8 installer contract. No subprocess, model or daemon.
#[path = "../src/test_temp.rs"]
mod test_temp;

use dot_agent_deck::opencode_manage;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

// All tests in this binary take this lock before changing the environment.
// No test starts background threads. nextest runs each in a separate process.
static ENV: Mutex<()> = Mutex::new(());
struct Install {
    saved: Vec<(&'static str, Option<OsString>)>,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    _tmp: tempfile::TempDir,
    _lock: MutexGuard<'static, ()>,
}
impl Install {
    fn new() -> Self {
        let lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = test_temp::tempdir().unwrap();
        let home = tmp.path().join("home");
        let xdg = home.join(".config");
        let root = xdg.join("opencode");
        std::fs::create_dir_all(&root).unwrap();
        let bin = home.join(".local/bin/dot-agent-deck");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut result = Self {
            saved: vec![],
            root,
            home,
            bin,
            _tmp: tmp,
            _lock: lock,
        };
        for (key, value) in [
            ("HOME", result.home.clone()),
            ("XDG_CONFIG_HOME", xdg),
            ("DOT_AGENT_DECK_TEST_CONFIG_WRITE", PathBuf::from("1")),
            (
                "DOT_AGENT_DECK_TEST_CONFIG_ROOT",
                result._tmp.path().to_path_buf(),
            ),
            ("PATH", result.bin.parent().unwrap().to_path_buf()),
        ] {
            result.set(key, value.into_os_string());
        }
        result
    }
    fn set(&mut self, key: &'static str, value: OsString) {
        if !self.saved.iter().any(|(k, _)| *k == key) {
            self.saved.push((key, std::env::var_os(key)));
        }
        // SAFETY: ENV excludes every sibling test; this fixture spawns no threads.
        unsafe {
            std::env::set_var(key, value);
        }
    }
    fn config(&self) -> PathBuf {
        self.root.join("tui.json")
    }
    fn plugin(&self) -> PathBuf {
        self.root.join("dot-agent-deck-tui.js")
    }
    fn entry(&self) -> String {
        serde_json::to_string(&self.plugin()).unwrap()
    }
    fn install(&self) {
        opencode_manage::install().expect("explicit OpenCode install");
    }
    fn seed(&self, text: impl AsRef<[u8]>) {
        std::fs::write(self.config(), text).unwrap();
    }
    fn text(&self) -> String {
        std::fs::read_to_string(self.config()).expect("S8 must create tui.json")
    }
    fn tui(&self) -> String {
        std::fs::read_to_string(self.plugin())
            .expect("S8 must install a separate root-level TUI plugin")
    }
    fn refuse(&self, bytes: &[u8]) {
        self.seed(bytes);
        let result = opencode_manage::install();
        assert_eq!(
            std::fs::read(self.config()).unwrap(),
            bytes,
            "refusal must preserve every byte"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "refusal left temp files");
        assert!(
            result.is_err(),
            "unsafe TUI config must be refused, rather than reported installed"
        );
    }
}
impl Drop for Install {
    fn drop(&mut self) {
        for (key, old) in self.saved.iter().rev() {
            // SAFETY: ENV remains held until this fixture's fields drop.
            unsafe {
                match old {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
fn fingerprint(path: &std::path::Path) -> (Vec<u8>, u64, i64, i64, u32) {
    let m = std::fs::metadata(path).unwrap();
    (
        std::fs::read(path).unwrap(),
        m.ino(),
        m.mtime(),
        m.mtime_nsec(),
        m.mode(),
    )
}

/// Scenario: Explicitly install into an empty XDG root. The config must list one separate TUI plugin whose binary pin points at the durable scratch install.
#[test]
fn opencode_install_fresh_tui_config_and_pinned_template() {
    let f = Install::new();
    f.install();
    let config: serde_json::Value = serde_json::from_str(&f.text()).unwrap();
    assert_eq!(config["$schema"], "https://opencode.ai/tui.json");
    assert_eq!(config["plugin"], serde_json::json!([f.plugin()]));
    let tui = f.tui();
    assert!(tui.contains(&format!(
        "const BINARY_PATH = {};",
        serde_json::to_string(&f.bin).unwrap()
    )));
    assert!(tui.contains("export default") && tui.contains("tui"));
    assert!(
        !tui.contains("server:"),
        "TUI plugin must not export a server plugin"
    );
    assert!(
        !tui.contains("appendPrompt")
            && !tui.contains("submitPrompt")
            && !tui.contains("clearPrompt")
    );
    assert!(!tui.contains("OPENCODE_TUI_CONFIG"));
}

/// Scenario: Install over JSONC with comments, user plugins and trailing commas. Only insertion immediately after the plugin array opener is permitted, with existing permission bits kept.
#[test]
fn opencode_install_jsonc_splices_only_entry_and_preserves_mode() {
    let f = Install::new();
    let before = "{\n  // user palette\n  \"theme\": \"dark\", /* keep */\n  \"plugin\": [\"user-a\", \"user-b\",],\n}\n";
    f.seed(before);
    std::fs::set_permissions(f.config(), std::fs::Permissions::from_mode(0o640)).unwrap();
    f.install();
    assert_eq!(
        f.text(),
        before.replacen("[", &format!("[{}, ", f.entry()), 1)
    );
    assert_eq!(std::fs::metadata(f.config()).unwrap().mode() & 0o777, 0o640);
}

/// Scenario: Install over empty and nonempty objects lacking a plugin key. Add exactly the first member, without a dangling comma in the empty case.
#[test]
fn opencode_install_missing_plugin_splices_first_member() {
    let f = Install::new();
    for before in ["{}", "{\"theme\":\"dark\"}"] {
        f.seed(before);
        f.install();
        let after = f.text();
        let value: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(value["plugin"], serde_json::json!([f.plugin()]));
        assert!(after.find("\"plugin\"").unwrap() < after.find("\"theme\"").unwrap_or(usize::MAX));
    }
}

/// Scenario: Install twice into a fresh root. The second install must keep bytes, inode, timestamp and mode of both generated files.
#[test]
fn opencode_install_second_install_is_a_complete_noop() {
    let f = Install::new();
    f.install();
    let _ = f.tui();
    let _ = f.text();
    let before = (fingerprint(&f.config()), fingerprint(&f.plugin()));
    f.install();
    assert_eq!((fingerprint(&f.config()), fingerprint(&f.plugin())), before);
}

/// Scenario: Install with only a commented tui.jsonc present. Add the entry there and keep the .json path absent.
#[test]
fn opencode_install_uses_existing_jsonc() {
    let f = Install::new();
    let path = f.root.join("tui.jsonc");
    let before = "{/* palette */\"theme\":\"dark\",\"plugin\":[\"user\",]}\n";
    std::fs::write(&path, before).unwrap();
    f.install();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        before.replacen("[", &format!("[{}, ", f.entry()), 1)
    );
    assert!(!f.config().exists());
}

/// Scenario: Keep the deck entry in tui.jsonc alongside a separate tui.json. Installing must recognise the entry in either file and leave both byte and metadata identical.
#[test]
fn opencode_install_entry_in_jsonc_is_already_installed() {
    let f = Install::new();
    f.seed("{\"theme\":\"dark\"}");
    let path = f.root.join("tui.jsonc");
    std::fs::write(&path, format!("{{\"plugin\":[{}]}}", f.entry())).unwrap();
    let before = (fingerprint(&f.config()), fingerprint(&path));
    f.install();
    let _ = f.tui();
    assert_eq!((fingerprint(&f.config()), fingerprint(&path)), before);
}

/// Scenario: List the deck's plugin by file URL next to a user entry merely containing its name. Install must avoid duplicates and uninstall must remove only the exact deck path, preserving comments and the user plugin.
#[test]
fn opencode_install_file_url_and_uninstall_exact_signature() {
    let f = Install::new();
    let entry = serde_json::to_string(&format!("file://{}", f.plugin().display())).unwrap();
    let before = format!("{{/*keep*/\"plugin\":[{entry},\"user-dot-agent-deck-theme\",]}}\n");
    f.seed(&before);
    f.install();
    let _ = f.tui();
    assert_eq!(f.text(), before);
    opencode_manage::uninstall().unwrap();
    assert_eq!(
        f.text(),
        "{/*keep*/\"plugin\":[\"user-dot-agent-deck-theme\",]}\n"
    );
    assert!(!f.plugin().exists());
}

/// Scenario: Uninstall from a user config with just a deck entry and comments. Leave an empty plugin list and all comments, and delete the TUI file.
#[test]
fn opencode_install_uninstall_keeps_empty_user_list() {
    let f = Install::new();
    f.seed(format!("{{/*keep*/\"plugin\":[{}]}}", f.entry()));
    f.install();
    let _ = f.tui();
    opencode_manage::uninstall().unwrap();
    assert_eq!(f.text(), "{/*keep*/\"plugin\":[]}");
    assert!(!f.plugin().exists());
}

/// Scenario: Install from nothing then uninstall. Delete both the generated config and the separate TUI file.
#[test]
fn opencode_install_uninstall_deletes_pristine_generated_config() {
    let f = Install::new();
    f.install();
    let _ = f.text();
    let _ = f.tui();
    opencode_manage::uninstall().unwrap();
    assert!(!f.config().exists() && !f.plugin().exists());
}

/// Scenario: Corrupt a previously installed config, then uninstall. Refuse the unsafe edit and retain the TUI file so OpenCode is never left with a dangling plugin entry.
#[test]
fn opencode_install_unsafe_uninstall_keeps_tui_file() {
    let f = Install::new();
    f.install();
    let _ = f.tui();
    f.seed("{broken");
    let before = fingerprint(&f.plugin());
    assert!(opencode_manage::uninstall().is_err());
    assert_eq!(fingerprint(&f.plugin()), before);
    assert_eq!(f.text(), "{broken");
}

/// Scenario: Automatically install with only the legacy OpenCode root present. Write the server plugin there, but create neither an XDG root nor a legacy TUI config or TUI plugin.
#[test]
fn opencode_install_tui_half_is_xdg_only_and_not_speculative() {
    let f = Install::new();
    std::fs::remove_dir(&f.root).unwrap();
    let legacy = f.home.join(".opencode");
    std::fs::create_dir_all(&legacy).unwrap();
    opencode_manage::auto_install();
    assert!(legacy.join("plugin/dot-agent-deck.js").exists());
    assert!(
        !f.root.exists()
            && !legacy.join("tui.json").exists()
            && !legacy.join("dot-agent-deck-tui.js").exists()
    );
    std::fs::create_dir_all(&f.root).unwrap();
    opencode_manage::auto_install();
    assert!(f.plugin().exists(), "existing XDG root must get TUI half");
}

/// Scenario: Restrict config writes to an unrelated owned root, then install, auto-install and uninstall. Every attempted TUI write or delete must be refused before changing the outside files.
#[test]
fn opencode_install_writers_respect_config_containment() {
    let mut f = Install::new();
    f.install();
    let _ = f.tui();
    let before = (fingerprint(&f.plugin()), fingerprint(&f.config()));
    let allowed = f._tmp.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    f.set("DOT_AGENT_DECK_TEST_CONFIG_ROOT", allowed.into_os_string());
    assert!(opencode_manage::install().is_err());
    opencode_manage::auto_install();
    assert!(opencode_manage::uninstall().is_err());
    assert_eq!((fingerprint(&f.plugin()), fingerprint(&f.config())), before);
}

/// Scenario: Attempt install over a symlinked TUI config. Refuse, retaining both the link and its target bytes.
#[test]
fn opencode_install_refuses_symlink() {
    let f = Install::new();
    let target = f.home.join("user.json");
    std::fs::write(&target, "{}").unwrap();
    symlink(&target, f.config()).unwrap();
    let result = opencode_manage::install();
    assert!(
        std::fs::symlink_metadata(f.config())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "{}");
    assert!(result.is_err(), "symlink must be refused");
}

/// Scenario: Attempt install over malformed JSONC. Refuse without changing bytes or leaving a temp file.
#[test]
fn opencode_install_refuses_invalid_jsonc() {
    Install::new().refuse(b"{/* unterminated");
}
/// Scenario: Attempt install over an array root. Refuse without changing bytes or leaving a temp file.
#[test]
fn opencode_install_refuses_nonobject_root() {
    Install::new().refuse(b"[]");
}
/// Scenario: Attempt install with duplicate root plugin keys. Refuse the ambiguous list without changing bytes.
#[test]
fn opencode_install_refuses_duplicate_plugin() {
    Install::new().refuse(b"{\"plugin\":[],\"plugin\":[]}");
}
/// Scenario: Attempt install with a string plugin value. Refuse without changing bytes or leaving a temp file.
#[test]
fn opencode_install_refuses_nonarray_plugin() {
    Install::new().refuse(b"{\"plugin\":\"user\"}");
}
/// Scenario: Attempt install with environment substitution in a string. Refuse without editing the pre-substitution configuration.
#[test]
fn opencode_install_refuses_env_substitution() {
    Install::new().refuse(b"{\"theme\":\"{env:THEME}\"}");
}
/// Scenario: Attempt install with file substitution in a string. Refuse without editing the pre-substitution configuration.
#[test]
fn opencode_install_refuses_file_substitution() {
    Install::new().refuse(b"{\"theme\":\"{file:theme.txt}\"}");
}
/// Scenario: Attempt install where only tui.plugin defines a list. Refuse the ambiguous flattened configuration without changing bytes.
#[test]
fn opencode_install_refuses_nested_plugin() {
    Install::new().refuse(b"{\"tui\":{\"plugin\":[]}}");
}
/// Scenario: Attempt install over non-UTF-8 data. Refuse without changing bytes or leaving a temp file.
#[test]
fn opencode_install_refuses_non_utf8() {
    Install::new().refuse(b"{\"theme\":\"\xff\"}");
}
/// Scenario: Attempt install over a file larger than one MiB. Refuse before editing or leaving a temp file.
#[test]
fn opencode_install_refuses_oversize() {
    Install::new().refuse(&vec![b' '; 1024 * 1024 + 1]);
}

/// Scenario: Attempt install over a directory named tui.json. Refuse the non-regular file without replacing it or leaving a temp file.
#[test]
fn opencode_install_refuses_nonregular_file() {
    let f = Install::new();
    std::fs::create_dir(f.config()).unwrap();
    assert!(
        opencode_manage::install().is_err(),
        "non-regular TUI config must be refused"
    );
    assert!(f.config().is_dir());
}

/// Scenario: Install with a durable binary path containing a quote and a backslash. Both generated modules must encode the exact path as a JSON string without injecting JavaScript.
#[test]
fn opencode_install_tui_binary_pin_is_json_escaped() {
    let mut f = Install::new();
    let special = f.home.join("quoted\"back\\slash/bin/dot-agent-deck");
    std::fs::create_dir_all(special.parent().unwrap()).unwrap();
    std::fs::rename(&f.bin, &special).unwrap();
    f.set("PATH", special.parent().unwrap().as_os_str().to_owned());
    f.bin = special;
    f.install();
    let literal = serde_json::to_string(&f.bin).unwrap();
    assert!(f.tui().contains(&format!("const BINARY_PATH = {literal};")));
    assert!(
        std::fs::read_to_string(f.root.join("plugin/dot-agent-deck.js"))
            .unwrap()
            .contains(&format!("const BINARY_PATH = {literal};"))
    );
}

/// Scenario: Automatically install over invalid JSONC under an existing XDG root. Leave the user's bytes intact and log exactly one warning naming that config and why no entry was installed.
#[test]
fn opencode_install_automatic_refusal_warns_once() {
    #[derive(Clone)]
    struct Capture(std::sync::Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let f = Install::new();
    f.seed("{broken");
    let logs = Capture(std::sync::Arc::new(Mutex::new(vec![])));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, opencode_manage::auto_install);
    assert_eq!(f.text(), "{broken");
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let named: Vec<_> = logs
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("tui.json"))
        .collect();
    assert_eq!(
        named.len(),
        1,
        "automatic refusal must warn once with the file and reason: {logs}"
    );
    assert!(
        f.plugin().exists(),
        "only the unsafe entry edit is refused; TUI file must be installed"
    );
}

/// Scenario: Parse the mini component wire name and round-trip it. A build with mini support must retain the name rather than fall back to an unknown component.
#[test]
fn opencode_mini_component_round_trips_wire_name() {
    let future: dot_agent_deck::event::InboxComponent =
        serde_json::from_str("\"future_unrecognised_component\"").unwrap();
    assert_eq!(
        future,
        dot_agent_deck::event::InboxComponent::Other,
        "unknown components remain forward-compatible"
    );
    let component: dot_agent_deck::event::InboxComponent =
        serde_json::from_str("\"opencode_mini\"").unwrap();
    assert_eq!(serde_json::to_value(component).unwrap(), "opencode_mini");
}

/// Scenario: Parse a mini repaint stream control and round-trip it. The repaint bit must survive decoding for the daemon to route the nudge.
#[test]
fn opencode_mini_repaint_control_round_trips() {
    let control: dot_agent_deck::event::InboxStreamControl =
        serde_json::from_str("{\"repaint\":true}").unwrap();
    assert_eq!(
        serde_json::to_value(control).unwrap()["repaint"],
        true,
        "stream decoder must retain repaint requests"
    );
}
