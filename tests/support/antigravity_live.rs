//! Isolated real-Antigravity authentication and onboarding for lane-2 tests.
use super::common;
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const TOKEN: &str = ".gemini/antigravity-cli/antigravity-oauth-token";

pub fn antigravity_test_model() -> String {
    std::env::var("DOT_AGENT_DECK_ANTIGRAVITY_TEST_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "gemini-3.8-flash-low".into())
}

fn host_token() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("host HOME")).join(TOKEN)
}

fn gemini_key() -> Option<String> {
    std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn token_redactions(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.len() >= 8 => out.push(s.clone()),
        Value::Object(map) => map.values().for_each(|v| token_redactions(v, out)),
        Value::Array(items) => items.iter().for_each(|v| token_redactions(v, out)),
        _ => {}
    }
}

pub fn seed_home(home: &Path, fixture: &Path) -> std::io::Result<()> {
    let source = host_token();
    let target = home.join(TOKEN);
    std::fs::create_dir_all(target.parent().unwrap())?;
    std::fs::set_permissions(
        target.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )?;
    if std::fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_file() && m.len() > 0) {
        let bytes = std::fs::read(source)?;
        // Register before a copy or a diagnostic can expose a token value.
        let mut redactions = vec![String::from_utf8_lossy(&bytes).into_owned()];
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            token_redactions(&value, &mut redactions);
        }
        common::register_diagnostic_redactions(redactions);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?
            .write_all(&bytes)?;
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o600
        );
    } else if gemini_key().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Antigravity token unavailable; sign in or set GEMINI_API_KEY",
        ));
    }
    let cli = home.join(".gemini/antigravity-cli");
    std::fs::create_dir_all(cli.join("cache"))?;
    std::fs::write(
        cli.join("cache/onboarding.json"),
        br#"{"onboardingComplete":true,"consumerOnboardingComplete":true}"#,
    )?;
    std::fs::write(
        cli.join("settings.json"),
        json!({"trustedWorkspaces":[fixture, fixture.canonicalize()?]}).to_string(),
    )?;
    Ok(())
}

/// Probe the same model the interactive pane uses, without an inference turn.
pub fn check_antigravity_available() -> Result<(), String> {
    common::install_credential_redaction();
    if !Command::new("agy")
        .arg("--version")
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return Err("Antigravity CLI not available on PATH (agy --version)".into());
    }
    let root = common::race_safe_tempdir();
    let home = root.path().join("home");
    seed_home(&home, root.path()).map_err(|_| {
        "Antigravity needs a non-empty OAuth token file or GEMINI_API_KEY".to_string()
    })?;
    let mut command = Command::new("agy");
    command
        .arg("models")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .current_dir(root.path())
        .stdin(Stdio::null());
    if let Some(key) = gemini_key() {
        command.env("GEMINI_API_KEY", key);
    }
    let child = std::cell::RefCell::new(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "Antigravity model availability probe failed".to_string())?,
    );
    if !common::wait_until(Duration::from_secs(30), || {
        child.borrow_mut().try_wait().is_ok_and(|s| s.is_some())
    }) {
        let _ = child.borrow_mut().kill();
        let _ = child.borrow_mut().wait();
        return Err("Antigravity model availability probe timed out".into());
    }
    let output = child
        .into_inner()
        .wait_with_output()
        .map_err(|_| "Antigravity model availability probe failed".to_string())?;
    let model = antigravity_test_model();
    if !output.status.success()
        || !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some(model.as_str()))
    {
        return Err(format!(
            "Antigravity model {model} unavailable; set DOT_AGENT_DECK_ANTIGRAVITY_TEST_MODEL to an accessible model"
        ));
    }
    eprintln!("Antigravity model under test: {model}");
    Ok(())
}
