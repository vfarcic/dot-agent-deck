//! PRD #2374: the `dot-agent-deck card` command group — `publish`, `list`,
//! `catalog` and `act`.
//!
//! `publish` runs inside an agent's pane and sends its report over the hook
//! socket, authenticated by the pane's hook capability token, and only after
//! the daemon confirms on the same connection that it stores cards. The other
//! three use the attach socket through [`DaemonClient`], which withholds each
//! request from a daemon that does not advertise
//! [`crate::daemon_protocol::CAP_AGENT_CARDS`].
//!
//! Exit codes, shared by all four: 0 done; 1 refused, invalid input or failed
//! (the message names a stable `[code]`); 2 the running daemon does not
//! support agent cards; 3 no deck daemon is running.

use std::io::Read;

use crate::agent_cards::{
    AgentReport, CODE_REPORT_INVALID, CODE_REPORT_TOO_LARGE, MAX_REPORT_BYTES, ReportAction,
    ReportError, ReportField, ReportStatus, split_key,
};
use crate::card_catalog::{FieldType, ResolvedCatalog};
use crate::close_cli::{EXIT_DAEMON_TOO_OLD, EXIT_OK, EXIT_REFUSED, EXIT_UNREACHABLE};
use crate::daemon_client::{ClientError, DaemonClient, GatedQuery};
use crate::roles_cli::CliOutput;

/// The code a refusal names when no deck daemon is running.
pub const CODE_NO_DAEMON: &str = "no_daemon";
/// The code a refusal names when the running daemon does not support cards.
pub const CODE_UNSUPPORTED: &str = "unsupported";
/// `card publish` outside a deck pane.
pub const CODE_NOT_IN_PANE: &str = "not_in_pane";
/// `card act` from an agent's pane.
pub const CODE_AGENT_CALLER: &str = "agent_caller";
/// The daemon took the request and its answer never arrived.
pub const CODE_UNCONFIRMED: &str = "unconfirmed";

fn error(sentence: impl std::fmt::Display, code: &str, exit: u8) -> CliOutput {
    CliOutput {
        stdout: String::new(),
        stderr: format!("Error: {sentence} [{code}]\n"),
        code: exit,
    }
}

fn ok(stdout: String) -> CliOutput {
    CliOutput {
        stdout,
        stderr: String::new(),
        code: EXIT_OK,
    }
}

fn no_daemon() -> CliOutput {
    error(
        "no deck daemon is running.",
        CODE_NO_DAEMON,
        EXIT_UNREACHABLE,
    )
}

fn unsupported(what: &str) -> CliOutput {
    error(
        format!("the running deck daemon does not support agent cards; upgrade it to {what}."),
        CODE_UNSUPPORTED,
        EXIT_DAEMON_TOO_OLD,
    )
}

/// Whether a client error means nothing answered at the attach socket.
fn is_unreachable(e: &ClientError) -> bool {
    match e {
        ClientError::SocketMissing(_) => true,
        ClientError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::AddrNotAvailable
        ),
        _ => false,
    }
}

fn client_error(e: ClientError) -> CliOutput {
    if is_unreachable(&e) {
        return no_daemon();
    }
    error(
        format!("the deck daemon could not answer: {e}"),
        "failed",
        EXIT_REFUSED,
    )
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

/// The input format of `card publish --file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Json,
    Toml,
}

impl ReportFormat {
    pub fn parse(raw: &str) -> Option<ReportFormat> {
        match raw {
            "json" => Some(ReportFormat::Json),
            "toml" => Some(ReportFormat::Toml),
            _ => None,
        }
    }
}

/// `card publish`'s arguments, as clap parsed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishArgs {
    pub file: Option<String>,
    pub format: Option<ReportFormat>,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub fields: Vec<String>,
    pub actions: Vec<String>,
    pub clear: bool,
    pub json: bool,
}

/// The format of `--file`: `--format` wins, else a `.toml` extension is TOML
/// and everything else (stdin included) JSON.
pub fn format_of(path: &str, format: Option<ReportFormat>) -> ReportFormat {
    format.unwrap_or_else(|| {
        if path != "-"
            && std::path::Path::new(path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("toml"))
        {
            ReportFormat::Toml
        } else {
            ReportFormat::Json
        }
    })
}

/// Build the report `card publish` sends from its arguments, reading
/// `--file` (or stdin for `-`), and check what does not depend on a catalog.
/// Which names are catalog entries, and so what their values must be, is the
/// daemon's to decide against the catalog it resolves cards with (product
/// decision (b)): `catalog` is that daemon's catalog when it could be read,
/// and is used only to type `--field NAME=VALUE` values.
pub fn build_report(
    args: &PublishArgs,
    catalog: Option<&ResolvedCatalog>,
    stdin: impl Read,
) -> Result<AgentReport, ReportError> {
    let report = if args.clear {
        AgentReport::default()
    } else if let Some(path) = &args.file {
        let raw = crate::bounded_read::read_task_input(path, stdin).map_err(|e| ReportError {
            code: CODE_REPORT_INVALID,
            message: e,
        })?;
        if raw.len() > MAX_REPORT_BYTES {
            return Err(ReportError {
                code: CODE_REPORT_TOO_LARGE,
                message: format!("the report is larger than {MAX_REPORT_BYTES} bytes"),
            });
        }
        match format_of(path, args.format) {
            ReportFormat::Json => AgentReport::parse_json(&raw)?,
            ReportFormat::Toml => AgentReport::parse_toml(&raw)?,
        }
    } else {
        report_from_flags(args, catalog)?
    };
    report.validate_schema()?;
    Ok(report)
}

fn invalid(message: impl Into<String>) -> ReportError {
    ReportError {
        code: CODE_REPORT_INVALID,
        message: message.into(),
    }
}

fn report_from_flags(
    args: &PublishArgs,
    catalog: Option<&ResolvedCatalog>,
) -> Result<AgentReport, ReportError> {
    let status = match &args.status {
        None => None,
        Some(raw) => Some(ReportStatus::parse(raw).ok_or_else(|| {
            invalid(format!(
                "status {raw:?} is not one of working, waiting, done, failed"
            ))
        })?),
    };
    let mut fields = Vec::new();
    for raw in &args.fields {
        let Some((name, value)) = raw.split_once('=') else {
            return Err(invalid(format!("--field {raw:?} is not NAME=VALUE")));
        };
        let field_type = catalog.and_then(|c| c.field(name)).map(|f| f.field_type);
        let value = match field_type {
            Some(FieldType::Number) => {
                let n: f64 = value
                    .trim()
                    .parse()
                    .map_err(|_| invalid(format!("field {name:?} must be a number")))?;
                serde_json::Number::from_f64(n)
                    .map(serde_json::Value::Number)
                    .ok_or_else(|| invalid(format!("field {name:?} must be a finite number")))?
            }
            Some(FieldType::Boolean) => match value {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                _ => return Err(invalid(format!("field {name:?} must be true or false"))),
            },
            Some(FieldType::List) => {
                return Err(invalid(format!(
                    "field {name:?} is a list; publish it with --file"
                )));
            }
            _ => serde_json::Value::String(value.to_string()),
        };
        fields.push(ReportField {
            name: name.to_string(),
            value,
            field_type: None,
            label: None,
        });
    }
    let actions = args
        .actions
        .iter()
        .map(|name| ReportAction {
            name: name.clone(),
            label: None,
            prompt: None,
            confirm: None,
        })
        .collect();
    Ok(AgentReport {
        status,
        summary: args.summary.clone(),
        fields,
        questions: Vec::new(),
        actions,
    })
}

/// The pane identity `card publish` sends with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneIdentity {
    pub pane_id: String,
    pub agent_id: Option<String>,
    pub token: Option<String>,
}

/// The pane identity from this process's environment, `None` outside a deck
/// pane.
pub fn pane_identity() -> Option<PaneIdentity> {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some(PaneIdentity {
        pane_id: read(crate::agent_pty::DOT_AGENT_DECK_PANE_ID)?,
        agent_id: read(crate::agent_pty::DOT_AGENT_DECK_AGENT_ID),
        token: crate::hook_provenance::token_from_env(),
    })
}

/// What `card publish` prints and exits with for the gated send's outcome.
pub fn publish_output(sent: crate::hook::Gated, json: bool) -> CliOutput {
    use crate::hook::Gated;
    let line = match sent {
        Gated::NotConfirmed {
            refusal: Some((code, message)),
            ..
        } => {
            return error(
                format!("the daemon did not accept this card: {message}"),
                &code,
                EXIT_REFUSED,
            );
        }
        Gated::NotConfirmed { why, .. } if why == crate::hook::GATED_NO_DAEMON => {
            return no_daemon();
        }
        Gated::NotConfirmed { .. } => {
            return unsupported("publish a card");
        }
        Gated::Unconfirmed(why) => {
            return error(
                format!("the deck daemon did not confirm this card was published ({why})"),
                CODE_UNCONFIRMED,
                EXIT_REFUSED,
            );
        }
        Gated::Reply(line) => line,
    };
    let Ok(reply) = serde_json::from_str::<crate::event::CardPublishReply>(line.trim()) else {
        return error(
            "the deck daemon's answer could not be read",
            CODE_UNCONFIRMED,
            EXIT_REFUSED,
        );
    };
    if !reply.accepted {
        let code = reply.code.unwrap_or_else(|| "refused".to_string());
        let message = reply.message.unwrap_or_default();
        let sentence = if code == CODE_REPORT_INVALID || code == CODE_REPORT_TOO_LARGE {
            format!("this card was not published: {message}")
        } else {
            format!("the daemon did not accept this card: {message}")
        };
        return error(sentence, &code, EXIT_REFUSED);
    }
    let revision = reply.revision.unwrap_or_default();
    if json {
        let body = serde_json::json!({
            "key": reply.key.unwrap_or_default(),
            "revision": revision,
        });
        ok(format!("{body}\n"))
    } else {
        ok(format!("Card published (revision {revision}).\n"))
    }
}

/// `card publish`. `daemon_catalog` asks the running daemon for the catalog
/// it resolves cards with; it is called only when `--field` flags need their
/// values typed, and answers `None` when the daemon could not say, when every
/// value is sent as text and the daemon's own check decides.
pub fn run_publish(
    args: &PublishArgs,
    daemon_catalog: impl FnOnce() -> Option<ResolvedCatalog>,
) -> CliOutput {
    let Some(identity) = pane_identity() else {
        return error(
            "card publish runs inside a dot-agent-deck pane; DOT_AGENT_DECK_PANE_ID is not set.",
            CODE_NOT_IN_PANE,
            EXIT_REFUSED,
        );
    };
    let catalog = (args.file.is_none() && !args.clear && !args.fields.is_empty())
        .then(daemon_catalog)
        .flatten();
    let report = match build_report(args, catalog.as_ref(), std::io::stdin()) {
        Ok(report) => report,
        Err(e) => {
            return error(
                format!("this card was not published: {}", e.message),
                e.code,
                EXIT_REFUSED,
            );
        }
    };
    let request = crate::event::CardPublishRequest {
        pane_id: identity.pane_id,
        agent_id: identity.agent_id,
        token: identity.token,
        report,
        timestamp: chrono::Utc::now(),
    };
    publish_output(crate::hook::send_card_publish(&request), args.json)
}

// ---------------------------------------------------------------------------
// list / catalog
// ---------------------------------------------------------------------------

/// `card list`: every live entry, as pretty-printed JSON.
pub async fn run_list(client: &DaemonClient) -> CliOutput {
    match client.list_agent_cards().await {
        Ok(GatedQuery::Answered(list)) => match serde_json::to_string_pretty(&list) {
            Ok(json) => ok(format!("{json}\n")),
            Err(e) => error(
                format!("the list could not be printed: {e}"),
                "failed",
                EXIT_REFUSED,
            ),
        },
        Ok(GatedQuery::Unsupported) => unsupported("list cards"),
        Err(e) => client_error(e),
    }
}

/// `card catalog`: the dispatch text block (or, with `json`, the resolved
/// catalog). Catalog errors are warnings and do not change the exit code.
pub async fn run_catalog(client: &DaemonClient, json: bool) -> CliOutput {
    match client.agent_card_catalog().await {
        Ok(GatedQuery::Answered(catalog)) => {
            if json {
                return match serde_json::to_string_pretty(&catalog) {
                    Ok(text) => ok(format!("{text}\n")),
                    Err(e) => error(
                        format!("the catalog could not be printed: {e}"),
                        "failed",
                        EXIT_REFUSED,
                    ),
                };
            }
            let bin = crate::platform::paths::binary_name();
            CliOutput {
                stdout: crate::card_catalog::dispatch_text(&catalog, &bin),
                stderr: catalog
                    .errors
                    .iter()
                    .map(|e| format!("warning: catalog: {e}\n"))
                    .collect(),
                code: EXIT_OK,
            }
        }
        Ok(GatedQuery::Unsupported) => unsupported("print the card catalog"),
        Err(e) => client_error(e),
    }
}

// ---------------------------------------------------------------------------
// act
// ---------------------------------------------------------------------------

/// `card act`'s arguments, as clap parsed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActArgs {
    pub entry: String,
    pub action: String,
    /// The action's `token` from `card list`.
    pub token: String,
    /// The list's `instance_id`: the daemon process that listed the card.
    pub instance: String,
    pub confirm: bool,
    pub text: Option<String>,
    pub text_file: Option<String>,
    pub json: bool,
}

/// A fresh random request id.
pub fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("card-{hex}")
}

/// What `card act` prints for the daemon's result.
pub fn act_output(result: &crate::agent_cards::CardActionResult, json: bool) -> CliOutput {
    if result.is_refused() {
        let code = result.code.as_deref().unwrap_or("refused");
        let message = result.message.as_deref().unwrap_or("refused");
        if json {
            let body = serde_json::to_string(result).unwrap_or_default();
            return CliOutput {
                stdout: format!("{body}\n"),
                stderr: format!("Error: {message} [{code}]\n"),
                code: EXIT_REFUSED,
            };
        }
        return error(message, code, EXIT_REFUSED);
    }
    if json {
        return ok(format!(
            "{}\n",
            serde_json::to_string(result).unwrap_or_default()
        ));
    }
    let name = result.message.as_deref().unwrap_or("the agent");
    match result.outcome.as_str() {
        "sent" => ok(format!(
            "Sent to {name}: message {} (queued).\n",
            result.message_id.as_deref().unwrap_or("?")
        )),
        "closed" => match &result.kept_worktree {
            Some(path) => ok(format!("Closed {name}, kept its worktree at {path}.\n")),
            None => ok(format!("Closed {name}.\n")),
        },
        other => ok(format!("{other}\n")),
    }
}

/// `card act`. `in_pane` is whether this process runs in an agent's pane.
pub async fn run_act(
    client: &DaemonClient,
    args: &ActArgs,
    in_pane: bool,
    stdin: impl Read,
) -> CliOutput {
    if in_pane {
        return error(
            "card act runs from the user's shell, not from an agent's pane.",
            CODE_AGENT_CALLER,
            EXIT_REFUSED,
        );
    }
    let Some((pane_id, agent_id)) = split_key(&args.entry) else {
        return error(
            format!("there is no card {}", args.entry),
            crate::agent_cards::CODE_UNKNOWN_ENTRY,
            EXIT_REFUSED,
        );
    };
    let text = match (&args.text, &args.text_file) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(path)) => match crate::bounded_read::read_task_input(path, stdin) {
            Ok(text) => Some(text),
            Err(e) => return error(e, crate::agent_cards::CODE_TEXT_REQUIRED, EXIT_REFUSED),
        },
        (None, None) => None,
    };
    let request_id = new_request_id();
    let target = crate::agent_cards::CardActionTarget {
        instance_id: args.instance.clone(),
        pane_id: pane_id.to_string(),
        agent_id: agent_id.to_string(),
        action_id: args.action.clone(),
        action_token: args.token.clone(),
    };
    match client
        .invoke_card_action(&target, &request_id, args.confirm, text.as_deref())
        .await
    {
        Ok(GatedQuery::Answered(result)) => act_output(&result, args.json),
        Ok(GatedQuery::Unsupported) => unsupported("act on cards"),
        Err(ClientError::DaemonRestarted | ClientError::DaemonUnnamed) => error(
            "the deck's daemon was restarted after this card was read; list the cards again",
            crate::agent_cards::CODE_DAEMON_REPLACED,
            EXIT_REFUSED,
        ),
        Err(e) => client_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card_catalog::resolve;

    #[test]
    fn flags_build_a_typed_report() {
        let catalog = resolve(Some(
            "[[catalog.fields]]\nname = \"build-time\"\ndescription = \"d\"\ntype = \"number\"\n",
        ));
        let args = PublishArgs {
            status: Some("waiting".into()),
            summary: Some("s".into()),
            fields: vec!["progress=a=b".into(), "build-time=42".into()],
            actions: vec!["merge".into()],
            ..PublishArgs::default()
        };
        let report = build_report(&args, Some(&catalog), std::io::empty()).unwrap();
        assert_eq!(report.status, Some(ReportStatus::Waiting));
        assert_eq!(report.fields[0].value, serde_json::json!("a=b"));
        assert_eq!(report.fields[1].value, serde_json::json!(42.0));
        assert_eq!(report.actions[0].name, "merge");
        let list = PublishArgs {
            fields: vec!["links=x".into()],
            ..PublishArgs::default()
        };
        assert!(build_report(&list, Some(&catalog), std::io::empty()).is_err());
        let clear = PublishArgs {
            clear: true,
            ..PublishArgs::default()
        };
        assert_eq!(
            build_report(&clear, Some(&catalog), std::io::empty()).unwrap(),
            AgentReport::default()
        );
    }

    /// Product decision (b): catalog membership is the daemon's to decide.
    /// The CLI sends a suggestion of a name its own environment does not
    /// know, a catalog-only suggestion with no label, and a report-typed
    /// field, refusing only what no catalog could accept.
    #[test]
    fn the_cli_leaves_catalog_membership_to_the_daemon() {
        let flags = PublishArgs {
            fields: vec!["build-time=42".into()],
            actions: vec!["ship".into(), "zap".into()],
            ..PublishArgs::default()
        };
        let report = build_report(&flags, None, std::io::empty()).unwrap();
        assert_eq!(report.fields[0].value, serde_json::json!("42"));
        assert_eq!(report.actions.len(), 2);
        let file = PublishArgs {
            file: Some("-".into()),
            ..PublishArgs::default()
        };
        let raw = r#"{"fields": [{"name": "risk", "value": 3}], "actions": [{"name": "deploy"}]}"#;
        assert!(build_report(&file, None, raw.as_bytes()).is_ok());
        let confirm =
            r#"{"actions": [{"name": "zap", "label": "Zap", "prompt": "p", "confirm": true}]}"#;
        assert_eq!(
            build_report(&file, None, confirm.as_bytes())
                .unwrap_err()
                .code,
            CODE_REPORT_INVALID
        );
        let reserved = r#"{"actions": [{"name": "close"}]}"#;
        assert!(build_report(&file, None, reserved.as_bytes()).is_err());
    }

    #[test]
    fn stdin_reads_json_unless_toml_is_asked() {
        let args = PublishArgs {
            file: Some("-".into()),
            ..PublishArgs::default()
        };
        let report = build_report(&args, None, r#"{"summary": "hi"}"#.as_bytes()).unwrap();
        assert_eq!(report.summary.as_deref(), Some("hi"));
        let args = PublishArgs {
            file: Some("-".into()),
            format: Some(ReportFormat::Toml),
            ..PublishArgs::default()
        };
        let report = build_report(&args, None, "summary = \"hi\"\n".as_bytes()).unwrap();
        assert_eq!(report.summary.as_deref(), Some("hi"));
        assert_eq!(format_of("r.toml", None), ReportFormat::Toml);
        assert_eq!(format_of("r.json", None), ReportFormat::Json);
        assert_eq!(
            format_of("r.toml", Some(ReportFormat::Json)),
            ReportFormat::Json
        );
    }

    #[test]
    fn publish_outcomes_map_to_exit_codes() {
        use crate::hook::Gated;
        let none = publish_output(
            Gated::NotConfirmed {
                refusal: None,
                why: crate::hook::GATED_NO_DAEMON,
            },
            false,
        );
        assert_eq!(none.code, EXIT_UNREACHABLE);
        assert_eq!(
            none.stderr,
            "Error: no deck daemon is running. [no_daemon]\n"
        );
        let old = publish_output(
            Gated::NotConfirmed {
                refusal: None,
                why: "the daemon did not confirm the verb",
            },
            false,
        );
        assert_eq!(old.code, EXIT_DAEMON_TOO_OLD);
        assert!(old.stderr.ends_with("[unsupported]\n"));
        let refused = publish_output(
            Gated::NotConfirmed {
                refusal: Some(("missing_token".into(), "refused: x".into())),
                why: "",
            },
            false,
        );
        assert_eq!(refused.code, EXIT_REFUSED);
        assert_eq!(
            refused.stderr,
            "Error: the daemon did not accept this card: refused: x [missing_token]\n"
        );
        let accepted = publish_output(
            Gated::Reply(r#"{"accepted":true,"key":"p/1","revision":7}"#.into()),
            false,
        );
        assert_eq!(accepted.stdout, "Card published (revision 7).\n");
        let json = publish_output(
            Gated::Reply(r#"{"accepted":true,"key":"p/1","revision":7}"#.into()),
            true,
        );
        assert_eq!(json.stdout, "{\"key\":\"p/1\",\"revision\":7}\n");
        let invalid = publish_output(
            Gated::Reply(r#"{"accepted":false,"code":"report_invalid","message":"m"}"#.into()),
            false,
        );
        assert_eq!(
            invalid.stderr,
            "Error: this card was not published: m [report_invalid]\n"
        );
    }

    #[test]
    fn request_ids_are_fresh() {
        assert_ne!(new_request_id(), new_request_id());
    }
}
