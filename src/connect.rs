//! PRD #76 M2.4 — `dot-agent-deck connect [name]` lookup + picker.
//!
//! After the 2026-05-09 architectural pivot (see PRD #76, "Architectural
//! pivot"), the laptop-side bridge that connected an in-process TUI to a
//! remote daemon over an ssh socket bridge has been removed (M2.7). What
//! survives in this module is the pure registry-side machinery:
//!
//! - **lookup** — `lookup_remote` resolves a name from the registry and
//!   rejects `kind = "kubernetes"` (planned in PRD #80, not yet supported).
//! - **picker** — `pick_remote` runs the numbered prompt when `<NAME>` is
//!   omitted; same kubernetes rejection applies.
//!
//! M2.9 will reintroduce a connect implementation built on `ssh -t` (the
//! TUI runs on the remote, not the laptop). Until then the CLI surface
//! exists but the handler stub in `main.rs` exits with a "not yet
//! implemented" message.

use std::io::{BufRead, Write};
use std::path::Path;

use thiserror::Error;

use crate::remote::{RemoteConfigError, RemoteEntry, RemotesFile};

/// Marker `kind` for entries the user added with `--type=kubernetes`. M2.4
/// rejects these explicitly so the message clearly points the user at PRD #80
/// instead of surfacing a generic ssh failure deeper in the connect path.
const KIND_KUBERNETES: &str = "kubernetes";

/// Maximum invalid attempts on the picker prompt before bailing. Three is the
/// usual ergonomic choice for a numeric prompt — the user gets two retries
/// after the first miss before we conclude they're not paying attention.
const PICKER_MAX_RETRIES: usize = 3;

#[derive(Debug, Error)]
pub enum RemoteConnectError {
    #[error(
        "No remote named '{name}'. Run `dot-agent-deck remote list` to see configured remotes."
    )]
    UnknownName { name: String },
    #[error(
        "Remote '{name}' is type 'kubernetes'; kubernetes remotes are not yet supported (planned in PRD #80)."
    )]
    KubernetesNotYetSupported { name: String },
    #[error("No remotes configured. Run `dot-agent-deck remote add <name> <host>` to add one.")]
    NoRemotesConfigured,
    #[error("Invalid selection after {attempts} attempts; aborting.")]
    PickerGaveUp { attempts: usize },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Resolve a remote by name from the registry at `path`. Errors with
/// `UnknownName` if the name is missing or `KubernetesNotYetSupported` if
/// the entry's kind is `kubernetes`.
pub fn lookup_remote(name: &str, path: &Path) -> Result<RemoteEntry, RemoteConnectError> {
    let registry = RemotesFile::load(path)?;
    let entry = registry
        .remotes
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| RemoteConnectError::UnknownName {
            name: name.to_string(),
        })?;
    if entry.kind == KIND_KUBERNETES {
        return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
    }
    Ok(entry)
}

/// Render one picker row. Kubernetes entries are listed but tagged so the
/// user knows they can't pick one yet.
fn format_picker_row(idx: usize, entry: &RemoteEntry) -> String {
    if entry.kind == KIND_KUBERNETES {
        format!(
            "  {idx}) {:<12} (kubernetes)   [PRD #80 — not yet connectable]\n",
            entry.name
        )
    } else {
        format!("  {idx}) {:<12} (ssh, {})\n", entry.name, entry.host)
    }
}

/// Run the env picker.
///
/// - 0 entries: error with the empty-state hint (this is a hard "can't
///   proceed" — distinct from `remote list`'s ambient empty state).
/// - 1 entry: auto-pick. Print "Connecting to <name>..." and return without
///   prompting. Kubernetes-only registry still routes through the PRD #80
///   rejection.
/// - 2+ entries: numbered prompt. Up to [`PICKER_MAX_RETRIES`] invalid
///   attempts before giving up.
///
/// Generic over `BufRead` / `Write` so tests can inject fake I/O.
pub fn pick_remote<R: BufRead, W: Write>(
    path: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<RemoteEntry, RemoteConnectError> {
    let registry = RemotesFile::load(path)?;
    if registry.remotes.is_empty() {
        return Err(RemoteConnectError::NoRemotesConfigured);
    }
    if registry.remotes.len() == 1 {
        let entry = registry.remotes.into_iter().next().expect("len==1");
        if entry.kind == KIND_KUBERNETES {
            return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
        }
        writeln!(output, "Connecting to {}...", entry.name)?;
        return Ok(entry);
    }

    writeln!(output, "Select a remote:")?;
    for (i, entry) in registry.remotes.iter().enumerate() {
        write!(output, "{}", format_picker_row(i + 1, entry))?;
    }

    let mut attempts = 0usize;
    loop {
        write!(output, "> ")?;
        output.flush()?;
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        // EOF without a valid pick is the same failure mode as an invalid
        // entry — bail rather than spin forever.
        if n == 0 {
            return Err(RemoteConnectError::PickerGaveUp {
                attempts: attempts + 1,
            });
        }
        let trimmed = line.trim();
        match trimmed.parse::<usize>() {
            Ok(n) if (1..=registry.remotes.len()).contains(&n) => {
                let entry = registry
                    .remotes
                    .into_iter()
                    .nth(n - 1)
                    .expect("bounds checked");
                if entry.kind == KIND_KUBERNETES {
                    return Err(RemoteConnectError::KubernetesNotYetSupported { name: entry.name });
                }
                return Ok(entry);
            }
            _ => {
                attempts += 1;
                if attempts >= PICKER_MAX_RETRIES {
                    return Err(RemoteConnectError::PickerGaveUp { attempts });
                }
                writeln!(
                    output,
                    "Please enter a number between 1 and {}.",
                    registry.remotes.len()
                )?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn entry(name: &str, kind: &str, host: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            kind: kind.to_string(),
            host: host.to_string(),
            port: 22,
            key: None,
            version: "0.24.5".to_string(),
            added_at: "2026-05-09T01:00:00+00:00".to_string(),
            upgraded_at: None,
        }
    }

    fn write_registry(dir: &tempfile::TempDir, entries: Vec<RemoteEntry>) -> PathBuf {
        let path = dir.path().join("remotes.toml");
        let file = RemotesFile { remotes: entries };
        file.save(&path).unwrap();
        path
    }

    // ----- lookup -----

    #[test]
    fn connect_lookup_unknown_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "viktor@hetzner-1.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        let err = lookup_remote("missing", &path).expect_err("unknown name must error");
        match &err {
            RemoteConnectError::UnknownName { name } => assert_eq!(name, "missing"),
            other => panic!("unexpected error: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("missing"), "msg should name the remote: {msg}");
        assert!(
            msg.contains("dot-agent-deck remote list"),
            "msg should hint at `remote list`: {msg}"
        );
    }

    #[test]
    fn connect_lookup_kubernetes_type_routes_to_prd_80() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![entry("k3s-prod", "kubernetes", "k3s-prod.example.com")],
        );
        let err = lookup_remote("k3s-prod", &path).expect_err("kubernetes type must error");
        match &err {
            RemoteConnectError::KubernetesNotYetSupported { name } => {
                assert_eq!(name, "k3s-prod");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("PRD #80"), "msg should mention PRD #80: {msg}");
    }

    // ----- picker -----

    #[test]
    fn connect_picker_empty_registry_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml"); // does not exist
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err =
            pick_remote(&path, &mut input, &mut output).expect_err("empty registry must error");
        assert!(matches!(err, RemoteConnectError::NoRemotesConfigured));
        let msg = err.to_string();
        assert!(
            msg.contains("No remotes configured"),
            "msg should give the empty-state hint: {msg}"
        );
        assert!(
            msg.contains("dot-agent-deck remote add"),
            "msg should point at `remote add`: {msg}"
        );
    }

    #[test]
    fn connect_picker_single_entry_auto_picks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(&dir, vec![entry("only", "ssh", "only.example.com")]);
        // Empty stdin: the picker MUST NOT consume from input when there's
        // only one entry — there's nothing to choose. Constructing the
        // cursor with no bytes means a hypothetical read_line would return 0
        // (EOF) and surface as PickerGaveUp; since we expect Ok, we know
        // read_line was never called.
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let chosen = pick_remote(&path, &mut input, &mut output).expect("auto-pick");
        assert_eq!(chosen.name, "only");
        let stdout = String::from_utf8(output).unwrap();
        assert!(
            stdout.contains("Connecting to only..."),
            "single-entry path should announce the connection: {stdout}"
        );
        assert!(
            !stdout.contains("Select a remote"),
            "single-entry path must not print the picker header: {stdout}"
        );
    }

    #[test]
    fn connect_picker_invalid_input_reprompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "viktor@hetzner-1.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        // First two lines are invalid; third line picks #2. The picker must
        // accept #2 after reprompting (proves retry/reprompt logic works).
        let mut input = Cursor::new(b"abc\n99\n2\n".to_vec());
        let mut output = Vec::<u8>::new();
        let chosen = pick_remote(&path, &mut input, &mut output).expect("third try succeeds");
        assert_eq!(chosen.name, "lab");
        let stdout = String::from_utf8(output).unwrap();
        // Should have re-prompted at least twice with the bounds hint.
        let reprompt_count = stdout.matches("Please enter a number").count();
        assert!(
            reprompt_count >= 2,
            "expected >=2 reprompts after two bad inputs, got {reprompt_count} in:\n{stdout}"
        );
    }

    #[test]
    fn connect_picker_max_retries_bails() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_registry(
            &dir,
            vec![
                entry("hetzner-1", "ssh", "h.example.com"),
                entry("lab", "ssh", "lab.local"),
            ],
        );
        let mut input = Cursor::new(b"abc\nxyz\nfoo\n".to_vec());
        let mut output = Vec::<u8>::new();
        let err =
            pick_remote(&path, &mut input, &mut output).expect_err("3 invalid inputs must bail");
        match err {
            RemoteConnectError::PickerGaveUp { attempts } => {
                assert_eq!(attempts, PICKER_MAX_RETRIES);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
