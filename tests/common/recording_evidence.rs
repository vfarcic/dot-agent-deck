//! Preserve completed recordings before the flat, reel-compatible files are
//! invalidated at launch. Read Claude metadata from the isolated session's
//! transcript, never from requested defaults in settings.

use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const RETAINED_RUNS: usize = 10;
const FAILURE_PROTECTION_SECS: u64 = 7 * 24 * 60 * 60;

pub(crate) fn archive_before_discard(dir: &Path) -> std::io::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    archive_recording(dir, now.as_secs(), now.subsec_nanos())?;
    prune_recordings(dir, now.as_secs(), RETAINED_RUNS, FAILURE_PROTECTION_SECS)
}

fn archive_recording(dir: &Path, now: u64, nanos: u32) -> std::io::Result<()> {
    let mut artifacts = Vec::new();
    for name in super::RECORDING_ARTIFACTS {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => artifacts.push(path),
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} is not a regular recording file",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if artifacts.is_empty() {
        return Ok(());
    }
    let runs = dir.join("runs");
    std::fs::create_dir_all(&runs)?;
    require_directory(&runs)?;
    let pending = tempfile::Builder::new()
        .prefix(".pending-")
        .tempdir_in(&runs)?;
    for artifact in artifacts {
        std::fs::copy(
            &artifact,
            pending.path().join(artifact.file_name().unwrap()),
        )?;
    }
    let suffix = pending.path().file_name().unwrap().to_string_lossy();
    let archived = runs.join(format!("run-{now:020}-{nanos:09}-{suffix}"));
    // Publish the archive only after its copies are complete. On error the
    // original flat files survive, and TempDir removes the partial copy.
    std::fs::rename(pending.path(), archived)?;
    Ok(())
}

/// A soft count cap: recent failed (or incomplete) runs are protected even
/// when that temporarily leaves more than `keep` archives. Expired failures
/// compete with successes by recording time, oldest first.
pub(crate) fn prune_recordings(
    dir: &Path,
    now: u64,
    keep: usize,
    protect_secs: u64,
) -> std::io::Result<()> {
    let runs_dir = dir.join("runs");
    match require_directory(&runs_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let entries = match std::fs::read_dir(dir.join("runs")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut runs = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_dir() || !name.starts_with("run-") {
            continue;
        }
        let Some(archived_at) = name.split('-').nth(1).and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        let provenance = std::fs::read(entry.path().join("provenance.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let recorded_at = provenance
            .as_ref()
            .and_then(|v| v["recorded_at_unix"].as_u64())
            .unwrap_or(archived_at);
        let passed = provenance
            .as_ref()
            .is_some_and(|v| v["outcome"] == "passed");
        let protected = !passed && now.saturating_sub(recorded_at) < protect_secs;
        runs.push((recorded_at, entry.path(), protected));
    }
    runs.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut excess = runs.len().saturating_sub(keep);
    for (_, path, protected) in runs {
        if excess == 0 {
            break;
        }
        if !protected {
            std::fs::remove_dir_all(path)?;
            excess -= 1;
        }
    }
    Ok(())
}

/// Extract metadata rather than persisting the transcript (which contains
/// prompts, tool output and possibly credentials). Unknown observations remain
/// null; settings and command-line model aliases are not evidence.
pub(crate) fn claude_session_provenance(home: &Path) -> Vec<Value> {
    let root = home.join(".claude").join("projects");
    let mut files = Vec::new();
    session_files(&root, 0, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let mut result = json!({
                "transcript": path.strip_prefix(home).unwrap_or(&path).to_string_lossy(),
                "session_id": null,
                "permissionMode": null,
                "permission_modes": [],
                "version": null,
            "models": [],
            "model": null,
            });
            match std::fs::File::open(&path) {
                Ok(mut file) => {
                    const TRANSCRIPT_LIMIT: u64 = 16 * 1024 * 1024;
                    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
                    let truncated = length > TRANSCRIPT_LIMIT;
                    result["truncated"] = json!(truncated);
                    if truncated
                        && let Err(error) = file.seek(SeekFrom::Start(length - TRANSCRIPT_LIMIT))
                    {
                        result["read_error"] = json!(error.kind().to_string());
                        return result;
                    }
                    let mut reader = std::io::BufReader::new(file);
                    let mut line = Vec::new();
                    if truncated {
                        // The window may begin in the middle of a JSON record.
                        let _ = reader.read_until(b'\n', &mut line);
                        line.clear();
                    }
                    while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                        if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                            for (source, target) in [
                                ("sessionId", "session_id"),
                                ("version", "version"),
                                ("permissionMode", "permissionMode"),
                            ] {
                                if let Some(value) = record[source].as_str() {
                                    result[target] = json!(value);
                                }
                            }
                            if let Some(mode) = record["permissionMode"].as_str() {
                                push_distinct(&mut result["permission_modes"], mode);
                            }
                            if let Some(model) = record["message"]["model"].as_str() {
                                result["model"] = json!(model);
                                push_distinct(&mut result["models"], model);
                            }
                        }
                        line.clear();
                    }
                }
                Err(error) => result["read_error"] = json!(error.kind().to_string()),
            }
            result
        })
        .collect()
}

fn push_distinct(values: &mut Value, value: &str) {
    let values = values.as_array_mut().unwrap();
    if !values.iter().any(|v| v == value) {
        values.push(json!(value));
    }
}

fn session_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 6 || files.len() >= 256 || require_directory(dir).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= 256 {
            break;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            session_files(&entry.path(), depth + 1, files);
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
            files.push(entry.path());
        }
    }
}

fn require_directory(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a regular recording directory",
            path.display()
        )))
    }
}
