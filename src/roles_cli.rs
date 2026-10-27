//! Issue #1947: `dot-agent-deck roles` and `dot-agent-deck inbox send` — find
//! an orchestration's orchestrator, and message it.
//!
//! `roles` uses the existing `ListAgents` command, so it does not need the
//! daemon to offer the addressed-message capability; its `--folder` /
//! `--orchestration` filters run the resolver the daemon sends with
//! ([`crate::orchestrator_address::select`]), over the same canonical folder
//! keys ([`crate::orchestrator_address::canonical_keys`]). `inbox send` goes
//! through [`DaemonClient::addressed_message`], which refuses a daemon that
//! does not offer the verb.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use serde::Serialize;

use crate::agent_pty::{AgentRecord, TabMembership};
use crate::close_cli::{
    AmbientIdentity, EXIT_DAEMON_TOO_OLD, EXIT_OK, EXIT_REFUSED, EXIT_UNREACHABLE,
};
use crate::daemon_client::{ClientError, CloseAgentsError, DaemonClient};
use crate::orchestrator_address::{
    AddressedMessageReport, MessageAddress, Selection, agent_name, canonical_keys, select,
};

/// The longest value one cell of the `roles` table shows.
const MAX_CELL_CHARS: usize = 512;

/// What a command prints and exits with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
}

impl CliOutput {
    fn stderr(message: String, code: u8) -> Self {
        Self {
            stdout: String::new(),
            stderr: format!("{message}\n"),
            code,
        }
    }
}

/// One row of `roles --json`. The keys are the command's contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleRow {
    pub pane_id: String,
    pub name: String,
    pub folder: String,
    /// The orchestration's display title, else its config name.
    pub orchestration: Option<String>,
    pub orchestration_name: Option<String>,
    pub orchestration_id: Option<String>,
    pub role: Option<String>,
    pub orchestrator: bool,
}

impl RoleRow {
    pub fn of(record: &AgentRecord) -> Self {
        let folder = record
            .cwd
            .clone()
            .or_else(|| crate::orchestrator_address::record_folder(record).map(str::to_string))
            .unwrap_or_default();
        let pane_id = record.pane_id_env.clone().unwrap_or_default();
        match &record.tab_membership {
            Some(TabMembership::Orchestration {
                name,
                role_name,
                is_start_role,
                display_title,
                orchestration_id,
                ..
            }) => Self {
                pane_id,
                name: record
                    .display_name
                    .clone()
                    .unwrap_or_else(|| role_name.clone()),
                folder,
                orchestration: Some(display_title.clone().unwrap_or_else(|| name.clone())),
                orchestration_name: Some(name.clone()),
                orchestration_id: orchestration_id.clone(),
                role: Some(role_name.clone()),
                orchestrator: *is_start_role,
            },
            _ => Self {
                pane_id,
                name: agent_name(record).to_string(),
                folder,
                orchestration: None,
                orchestration_name: None,
                orchestration_id: None,
                role: None,
                orchestrator: false,
            },
        }
    }
}

/// `folder` made absolute against `cwd`, and otherwise as spelled: the daemon
/// canonicalises, and may not share this machine's view of the path.
pub fn absolute_folder(folder: &str, cwd: &Path) -> String {
    let path = Path::new(folder);
    if path.is_absolute() {
        folder.to_string()
    } else {
        cwd.join(path).to_string_lossy().into_owned()
    }
}

/// The records `address` selects — every member of each orchestration it
/// matches, or the standalone agents it falls back to. With no address, all.
/// `keys` maps a folder to the key it is compared by, as
/// [`canonical_keys`] answers it; a folder it does not hold is its own key.
pub fn filter<'a>(
    records: &'a [AgentRecord],
    address: &MessageAddress,
    keys: &HashMap<String, String>,
) -> Vec<&'a AgentRecord> {
    if address.folder.is_none() && address.orchestration.is_none() {
        return records.iter().collect();
    }
    let key = |folder: &str| {
        keys.get(folder)
            .cloned()
            .unwrap_or_else(|| folder.to_string())
    };
    match select(records, address, &key, None) {
        Selection::Orchestrations(found) => found.into_iter().flat_map(|o| o.members).collect(),
        Selection::Standalone(found) => found,
        Selection::Nothing => Vec::new(),
    }
}

/// `json` with DEL, every C1 control and every bidi format character written
/// as a `\uXXXX` escape. `serde_json` escapes the C0 controls a JSON string
/// must not hold, and writes these literally, where a terminal printing the
/// listing would act on them (a right-to-left override reorders the fields
/// after it). They are not JSON syntax, so in serialised JSON they occur only
/// inside strings, where the escape reads back as the same character.
pub fn escape_json_format_chars(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if ('\u{7f}'..='\u{9f}').contains(&c) || crate::untrusted_text::is_bidi_format_char(c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    out
}

fn cell(value: &str) -> String {
    crate::config_validation::escape_field_for_log(value, MAX_CELL_CHARS)
}

/// The `roles` table: a header row, then one row per agent.
pub fn render_table(rows: &[RoleRow]) -> String {
    let header = [
        "PANE",
        "NAME",
        "FOLDER",
        "ORCHESTRATION",
        "ROLE",
        "ORCHESTRATOR",
    ];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|row| {
            [
                cell(&row.pane_id),
                cell(&row.name),
                cell(&row.folder),
                cell(row.orchestration.as_deref().unwrap_or("-")),
                cell(row.role.as_deref().unwrap_or("-")),
                if row.orchestrator { "yes" } else { "" }.to_string(),
            ]
        })
        .collect();
    let width = |i: usize| {
        cells
            .iter()
            .map(|c| c[i].chars().count())
            .chain([header[i].len()])
            .max()
            .unwrap_or(0)
    };
    let widths: Vec<usize> = (0..header.len()).map(width).collect();
    let line = |values: &[&str]| {
        let mut out = String::new();
        for (i, value) in values.iter().enumerate() {
            if i + 1 == values.len() {
                out.push_str(value);
            } else {
                out.push_str(value);
                out.push_str(&" ".repeat(widths[i] - value.chars().count() + 2));
            }
        }
        format!("{}\n", out.trim_end())
    };
    let mut text = line(&header);
    for row in &cells {
        text.push_str(&line(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    text
}

/// The rows `roles` lists for `address`.
///
/// The daemon canonicalises the address and the record folders with
/// [`canonical_keys`] before it resolves a send, and so does this, so `..`
/// and a symlink select here what a send selects. That holds when both
/// resolve paths on one filesystem, as they do for `roles` and `inbox send`
/// run in a deck pane on the daemon's own machine, which is where the daemon
/// spawns its agents. A `roles` run against a daemon on another machine
/// canonicalises here, and may differ from that daemon for an alias
/// spelling; a folder neither side can resolve keys as spelled on both.
pub async fn select_rows(records: &[AgentRecord], address: &MessageAddress) -> Vec<RoleRow> {
    let keys = canonical_keys(records, address).await;
    filter(records, address, &keys)
        .into_iter()
        .map(RoleRow::of)
        .collect()
}

/// `dot-agent-deck roles`.
pub async fn run_roles(client: &DaemonClient, json: bool, address: &MessageAddress) -> CliOutput {
    let records = match client.list_agents().await {
        Ok(records) => records,
        Err(error) => return list_error_output(&error),
    };
    let rows = select_rows(&records, address).await;
    let stdout = if json {
        match serde_json::to_string_pretty(&rows) {
            Ok(text) => format!("{}\n", escape_json_format_chars(&text)),
            Err(error) => {
                return CliOutput::stderr(format!("roles: {error}"), EXIT_REFUSED);
            }
        }
    } else {
        render_table(&rows)
    };
    CliOutput {
        stdout,
        stderr: String::new(),
        code: EXIT_OK,
    }
}

fn list_error_output(error: &ClientError) -> CliOutput {
    match error {
        ClientError::SocketMissing(_) | ClientError::Io(_) => CliOutput::stderr(
            format!("roles: no daemon is running ({error})"),
            EXIT_UNREACHABLE,
        ),
        other => CliOutput::stderr(format!("roles: {other}"), EXIT_REFUSED),
    }
}

/// The text of an `inbox send`: `--text`, or `--file` (`-` reads `stdin`).
pub fn read_body(
    text: Option<String>,
    file: Option<String>,
    stdin: impl Read,
) -> Result<String, String> {
    match (text, file) {
        (Some(text), None) => Ok(text),
        (None, Some(path)) => crate::bounded_read::read_task_input(&path, stdin),
        _ => Err("give the text with exactly one of --text or --file".to_string()),
    }
}

/// `dot-agent-deck inbox send`. `address.folder` is already absolute.
pub async fn run_send(
    client: &DaemonClient,
    identity: &AmbientIdentity,
    address: MessageAddress,
    body: String,
) -> CliOutput {
    let Some(claim) = identity.claim() else {
        return CliOutput::stderr(
            "refused: `inbox send` runs from an agent's deck pane, and this shell is not one; \
             nothing was sent. A person messages an orchestrator by typing into its pane."
                .to_string(),
            EXIT_REFUSED,
        );
    };
    if address.folder.is_none() && address.orchestration.is_none() {
        return CliOutput::stderr(
            "refused: give --folder, --orchestration, or both; nothing was sent.".to_string(),
            EXIT_REFUSED,
        );
    }
    match client.addressed_message(address, claim, body).await {
        Ok(report) => render_report(&report),
        Err(error) => client_error_output(&error),
    }
}

/// What a report prints, and its exit code.
pub fn render_report(report: &AddressedMessageReport) -> CliOutput {
    match report {
        AddressedMessageReport::Sent {
            id,
            pane_id,
            orchestration,
            title,
            folder,
        } => {
            let to = match orchestration {
                Some(_) => format!("the orchestrator of {}", cell(title)),
                None => cell(title),
            };
            CliOutput {
                stdout: format!(
                    "Sent {} to {to} (pane {}) in {}.\n",
                    cell(id),
                    cell(pane_id),
                    cell(folder)
                ),
                stderr: String::new(),
                code: EXIT_OK,
            }
        }
        AddressedMessageReport::Refused {
            message,
            candidates,
            ..
        } => {
            let mut stderr = format!("{}\n", cell(message));
            for c in candidates {
                let what = match (&c.title, &c.orchestration) {
                    (Some(title), Some(name)) => format!(
                        "{} (orchestration {}) in {}: orchestrator pane {}",
                        cell(title),
                        cell(name),
                        cell(&c.folder),
                        if c.pane_id.is_empty() {
                            "none live".to_string()
                        } else {
                            cell(&c.pane_id)
                        }
                    ),
                    _ => format!(
                        "{} in {}: pane {}",
                        cell(&c.name),
                        cell(&c.folder),
                        cell(&c.pane_id)
                    ),
                };
                stderr.push_str(&format!("  {what}\n"));
            }
            CliOutput {
                stdout: String::new(),
                stderr,
                code: EXIT_REFUSED,
            }
        }
    }
}

/// The output for a send that got no report.
pub fn client_error_output(error: &CloseAgentsError) -> CliOutput {
    match error {
        CloseAgentsError::TooOld { daemon_version } => CliOutput::stderr(
            format!(
                "inbox send: the running daemon{} is too old for `inbox send`; nothing was \
                 sent. Restart it onto this build with `dot-agent-deck daemon restart`.",
                daemon_version
                    .as_deref()
                    .map(|v| format!(" ({})", cell(v)))
                    .unwrap_or_default()
            ),
            EXIT_DAEMON_TOO_OLD,
        ),
        CloseAgentsError::Unreachable(e) => CliOutput::stderr(
            format!("inbox send: no daemon is running ({e}); nothing was sent"),
            EXIT_UNREACHABLE,
        ),
        CloseAgentsError::Client(e) => {
            CliOutput::stderr(format!("refused: {}", cell(&e.to_string())), EXIT_REFUSED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator_address::{AddressCandidate, AddressRefusalReason};

    fn record(value: serde_json::Value) -> AgentRecord {
        serde_json::from_value(value).unwrap()
    }

    fn records() -> Vec<AgentRecord> {
        let member = |pane: &str, seat: bool| {
            record(serde_json::json!({
                "id": format!("agent-{pane}"),
                "pane_id_env": pane,
                "cwd": "/w/team",
                "tab_membership": {
                    "kind": "orchestration",
                    "name": "cfg",
                    "role_index": usize::from(!seat),
                    "role_name": if seat { "lead" } else { "worker" },
                    "is_start_role": seat,
                    "orchestration_cwd": "/w/team",
                    "display_title": "cfg · team",
                    "orchestration_id": "inst-1",
                },
            }))
        };
        vec![
            member("p-o", true),
            member("p-w", false),
            record(serde_json::json!({
                "id": "agent-solo",
                "pane_id_env": "p-s",
                "display_name": "solo",
                "cwd": "/w/solo",
            })),
        ]
    }

    #[test]
    fn rows_carry_the_contract_keys_and_mark_only_the_seat() {
        let rows: Vec<RoleRow> = records().iter().map(RoleRow::of).collect();
        let json = serde_json::to_value(&rows).unwrap();
        let keys: Vec<&str> = json[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 8, "{keys:?}");
        assert_eq!(json[0]["orchestrator"], true);
        assert_eq!(json[0]["role"], "lead");
        assert_eq!(json[0]["orchestration"], "cfg · team");
        assert_eq!(json[0]["orchestration_name"], "cfg");
        assert_eq!(json[0]["orchestration_id"], "inst-1");
        assert_eq!(json[1]["orchestrator"], false);
        assert!(json[2]["orchestration"].is_null());
        assert!(json[2]["role"].is_null());
        assert_eq!(json[2]["name"], "solo");
    }

    #[test]
    fn the_table_has_a_header_and_dashes_for_a_standalone_agent() {
        let rows: Vec<RoleRow> = records().iter().map(RoleRow::of).collect();
        let text = render_table(&rows);
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
            [
                "PANE",
                "NAME",
                "FOLDER",
                "ORCHESTRATION",
                "ROLE",
                "ORCHESTRATOR"
            ]
        );
        let seat = lines.next().unwrap();
        assert!(seat.starts_with("p-o "), "{seat}");
        assert_eq!(seat.split_whitespace().last(), Some("yes"));
        let worker = lines.next().unwrap();
        assert_eq!(worker.split_whitespace().last(), Some("worker"));
        let solo = lines.next().unwrap();
        assert_eq!(solo.split_whitespace().filter(|v| *v == "-").count(), 2);
    }

    #[test]
    fn filters_run_the_send_resolver() {
        let records = records();
        let panes = |address: MessageAddress| -> Vec<String> {
            filter(&records, &address, &HashMap::new())
                .into_iter()
                .map(|r| r.pane_id_env.clone().unwrap())
                .collect()
        };
        let at = |folder: &str| MessageAddress {
            folder: Some(folder.to_string()),
            orchestration: None,
        };
        assert_eq!(panes(at("/w/team/src")), ["p-o", "p-w"]);
        assert_eq!(panes(at("/w/solo")), ["p-s"]);
        assert!(panes(at("/w/tea")).is_empty(), "never by substring");
        assert_eq!(
            panes(MessageAddress {
                folder: Some("/w/team".to_string()),
                orchestration: Some("other".to_string()),
            }),
            Vec::<String>::new(),
            "the two filters intersect"
        );
        assert_eq!(panes(MessageAddress::default()).len(), 3);
    }

    /// Review 1 (A4): `roles --folder` lists what `inbox send` reaches with
    /// the same folder, for a `..` spelling and a symlink too. Two teams in
    /// `alpha/` and `beta/`; `alpha/../beta` and a link to `beta` both list
    /// beta, the team the daemon's resolver sends to with the same keys.
    #[tokio::test]
    async fn a_dotdot_or_symlinked_folder_lists_what_a_send_reaches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let team = |name: &str| {
            let folder = root.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            let folder = folder.to_string_lossy().into_owned();
            [true, false].map(|seat| {
                record(serde_json::json!({
                    "id": format!("agent-{name}-{seat}"),
                    "pane_id_env": format!("{name}-{}", if seat { "o" } else { "w" }),
                    "cwd": folder,
                    "tab_membership": {
                        "kind": "orchestration",
                        "name": name,
                        "role_index": usize::from(!seat),
                        "role_name": if seat { "lead" } else { "worker" },
                        "is_start_role": seat,
                        "orchestration_cwd": folder,
                        "orchestration_id": format!("{name}-1"),
                    },
                }))
            })
        };
        let records: Vec<AgentRecord> = team("alpha").into_iter().chain(team("beta")).collect();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("beta"), root.join("link")).unwrap();
        let mut spellings = vec![root.join("alpha/../beta")];
        if cfg!(unix) {
            spellings.push(root.join("link"));
        }
        for folder in spellings {
            let address = MessageAddress {
                folder: Some(folder.to_string_lossy().into_owned()),
                orchestration: None,
            };
            let listed: Vec<String> = select_rows(&records, &address)
                .await
                .into_iter()
                .map(|row| row.pane_id)
                .collect();
            assert_eq!(listed, ["beta-o", "beta-w"], "{folder:?}");
            let keys = canonical_keys(&records, &address).await;
            let key = |f: &str| keys.get(f).cloned().unwrap_or_else(|| f.to_string());
            let sent_to = crate::orchestrator_address::resolve(&records, &address, &key, "caller")
                .unwrap()
                .target()
                .pane_id_env
                .clone();
            assert_eq!(sent_to.as_deref(), Some("beta-o"), "{folder:?}");
        }
    }

    /// Audit 5 (A5): `roles --json` writes no literal bidi or C1 character,
    /// and still parses back to the exact folder.
    #[test]
    fn json_output_escapes_bidi_and_c1_and_round_trips_the_value() {
        let folder = "/w/a\u{202e}b\u{85}c\u{7f}d\u{2067}e";
        let row = RoleRow::of(&record(serde_json::json!({
            "id": "agent-s",
            "pane_id_env": "p-s",
            "cwd": folder,
        })));
        let raw = serde_json::to_string_pretty(&[row]).unwrap();
        assert!(raw.contains('\u{202e}'), "serde_json writes it literally");
        let text = escape_json_format_chars(&raw);
        for c in ['\u{202e}', '\u{85}', '\u{7f}', '\u{2067}'] {
            assert!(!text.contains(c), "U+{:04X} left literal", u32::from(c));
        }
        assert!(text.contains("\\u202e"), "{text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0]["folder"], folder);
    }

    #[test]
    fn a_relative_folder_is_made_absolute_and_not_canonicalised() {
        let cwd = Path::new("/w/here");
        assert_eq!(absolute_folder(".", cwd), "/w/here/.");
        assert_eq!(absolute_folder("../x", cwd), "/w/here/../x");
        assert_eq!(absolute_folder("/abs", cwd), "/abs");
    }

    #[test]
    fn a_body_comes_from_text_a_file_or_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("body.txt");
        std::fs::write(&file, "from a file").unwrap();
        assert_eq!(
            read_body(Some("inline".to_string()), None, std::io::empty()).unwrap(),
            "inline"
        );
        assert_eq!(
            read_body(
                None,
                Some(file.to_string_lossy().into_owned()),
                std::io::empty()
            )
            .unwrap(),
            "from a file"
        );
        assert_eq!(
            read_body(None, Some("-".to_string()), "from stdin".as_bytes()).unwrap(),
            "from stdin"
        );
        assert!(read_body(None, None, std::io::empty()).is_err());
    }

    #[test]
    fn a_report_prints_the_sentence_and_the_exit_code() {
        let sent = render_report(&AddressedMessageReport::Sent {
            id: "m-abc".to_string(),
            pane_id: "p-o".to_string(),
            orchestration: Some("cfg".to_string()),
            title: "cfg · team".to_string(),
            folder: "/w/team".to_string(),
        });
        assert_eq!(sent.code, EXIT_OK);
        assert_eq!(
            sent.stdout,
            "Sent m-abc to the orchestrator of cfg · team (pane p-o) in /w/team.\n"
        );
        let solo = render_report(&AddressedMessageReport::Sent {
            id: "m-abc".to_string(),
            pane_id: "p-s".to_string(),
            orchestration: None,
            title: "solo".to_string(),
            folder: "/w/solo".to_string(),
        });
        assert_eq!(solo.stdout, "Sent m-abc to solo (pane p-s) in /w/solo.\n");
        let refused = render_report(&AddressedMessageReport::Refused {
            reason: AddressRefusalReason::Ambiguous,
            message: "refused: ambiguous: 2 live orchestrations match".to_string(),
            candidates: vec![AddressCandidate {
                pane_id: "p-o".to_string(),
                name: "cfg".to_string(),
                folder: "/w/team".to_string(),
                orchestration: Some("cfg".to_string()),
                title: Some("cfg · team".to_string()),
            }],
        });
        assert_eq!(refused.code, EXIT_REFUSED);
        assert!(refused.stderr.starts_with("refused: ambiguous"));
        assert!(refused.stderr.contains("orchestrator pane p-o"));
    }

    #[tokio::test]
    async fn a_shell_with_no_pane_identity_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let client = DaemonClient::new(dir.path().join("no-daemon.sock"));
        let output = run_send(
            &client,
            &AmbientIdentity::Person,
            MessageAddress {
                folder: Some("/w".to_string()),
                orchestration: None,
            },
            "hi".to_string(),
        )
        .await;
        assert_eq!(output.code, EXIT_REFUSED);
        assert!(output.stderr.starts_with("refused: "), "{}", output.stderr);
    }

    #[test]
    fn a_daemon_without_the_verb_is_too_old_and_one_that_is_gone_is_unreachable() {
        let old = client_error_output(&CloseAgentsError::TooOld {
            daemon_version: Some("0.1.0".to_string()),
        });
        assert_eq!(old.code, EXIT_DAEMON_TOO_OLD);
        assert!(old.stderr.contains("too old"), "{}", old.stderr);
        let gone = client_error_output(&CloseAgentsError::Unreachable(std::io::Error::other(
            "refused",
        )));
        assert_eq!(gone.code, EXIT_UNREACHABLE);
    }
}
