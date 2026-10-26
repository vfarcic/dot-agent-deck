//! `dot-agent-deck hooks refresh` (PRD #1666 part 3, R-3): the automatic hook
//! install the deck runs when it starts, run on demand, and a report of what
//! every installed agent's hooks run afterwards, in words both clients show.
//!
//! An upgrade that installs a CLI runs it from the installed copy
//! ([`crate::self_upgrade::agent_hooks`]), so the agents' hooks do not stay on
//! an older dot-agent-deck until the deck next starts. Being the startup
//! install, it keeps what that keeps: another live, durable install's entry
//! stays unless this copy may take it over and it reports an older release
//! ([`crate::hook_binary::Takeover`]). What it leaves in place it says, with the
//! command that pins this copy instead.

use serde::{Deserialize, Serialize};

use crate::event::AgentType;
use crate::hook_binary::{DeckIdentity, HookPin};

/// One line of a [`RefreshReport`]: words, or a command to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshLine {
    pub text: String,
    /// When set, the line is this command, which `text` repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// What `hooks refresh --json` prints: whether every installed agent's hooks
/// are settled (`ok`), and what to tell the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshReport {
    pub ok: bool,
    #[serde(default)]
    pub lines: Vec<RefreshLine>,
}

impl RefreshReport {
    /// The report as a terminal prints it: a command indented by two spaces.
    pub fn render(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| match &line.command {
                Some(command) => format!("  {command}"),
                None => line.text.clone(),
            })
            .collect()
    }
}

/// What one agent's startup install came to, run for `hooks refresh`
/// ([`crate::agent_registry::AgentSpec::refresh_install`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAttempt {
    /// The agent is not installed here, so there was nothing to update.
    Absent,
    /// The install ran; `failed` says what it could not read or write.
    Ran { failed: Vec<String> },
}

impl InstallAttempt {
    /// From an install that is `None` when the agent is not installed, and
    /// otherwise what it installed or why it installed nothing.
    pub fn of<T>(attempt: Option<Result<T, String>>) -> Self {
        match attempt {
            None => Self::Absent,
            Some(Ok(_)) => Self::Ran { failed: Vec::new() },
            Some(Err(why)) => Self::Ran { failed: vec![why] },
        }
    }
}

/// One installed agent after its install: what the install could not do,
/// and what its config names now (`None` when it could not be read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Installed {
    pub agent: AgentType,
    pub failed: Vec<String>,
    pub pins: Option<Vec<HookPin>>,
}

/// Run every agent's startup install, then report what each installed agent's
/// hooks run. An agent that is not installed is left out; one whose install
/// failed, or whose config cannot be read back, needs attention.
pub fn refresh() -> RefreshReport {
    use crate::agent_registry::ALL;
    let ran: Vec<_> = ALL
        .iter()
        .filter_map(|spec| match (spec.refresh_install?)() {
            InstallAttempt::Absent => None,
            InstallAttempt::Ran { failed } => Some((spec, failed)),
        })
        .collect();
    let deck = DeckIdentity::current(crate::hook_binary::process_resolution());
    let installed = ran
        .into_iter()
        .map(|(spec, failed)| Installed {
            agent: spec.agent_type.clone(),
            failed,
            pins: spec.configured_pins.and_then(|read| read()),
        })
        .collect();
    report(
        &deck,
        installed,
        |binary| crate::hook_binary::names_this_deck(&deck, binary),
        |binary| {
            std::path::Path::new(binary)
                .is_absolute()
                .then(|| crate::hook_binary::probe_version(std::path::Path::new(binary)).ok())
                .flatten()
        },
    )
}

/// What the TUI's startup install came to, for the note the TUI shows as it
/// starts (PRD #91): `status` for its status line, and `warnings` for what it
/// prints when it exits, which outlasts the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupNote {
    pub status: String,
    pub warnings: Vec<String>,
}

/// Run every agent's startup install, as the TUI does when it starts, and say
/// what the user should hear about it: `None` when it changed nothing and
/// nothing failed.
///
/// Runs each agent's [`refresh_install`](crate::agent_registry::AgentSpec::refresh_install),
/// which is its startup install reporting what it could not do, so a settings
/// file the install refused to overwrite is named rather than only logged.
pub fn startup_install() -> Option<StartupNote> {
    use crate::agent_registry::ALL;
    let mut changed = Vec::new();
    let mut failed = Vec::new();
    for spec in ALL {
        let (attempt, wrote) = crate::agent_hook_config::noting_auto_install_changes(|| {
            match (spec.refresh_install, spec.startup_auto_install) {
                (Some(install), _) => install(),
                (None, Some(install)) => {
                    install();
                    InstallAttempt::Absent
                }
                (None, None) => InstallAttempt::Absent,
            }
        });
        match attempt {
            InstallAttempt::Ran { failed: why } if !why.is_empty() => {
                failed.push((spec.agent_type.clone(), why));
            }
            _ if wrote => changed.push(spec.agent_type.clone()),
            _ => {}
        }
    }
    startup_note(&changed, &failed)
}

/// The note for a startup install that changed the hooks of the `changed`
/// agents and could not update those in `failed`, with why. A failure is what
/// the status line leads with, since it is the one the user has to act on.
pub(crate) fn startup_note(
    changed: &[AgentType],
    failed: &[(AgentType, Vec<String>)],
) -> Option<StartupNote> {
    if let Some((agent, why)) = failed.first() {
        let first = could_not_update(agent, why);
        let status = match failed.len() {
            1 => first,
            n => format!("{first} (+{} more, printed on exit)", n - 1),
        };
        let mut warnings: Vec<String> = failed
            .iter()
            .map(|(agent, why)| could_not_update(agent, why))
            .collect();
        warnings.push(STARTUP_FIX.to_string());
        return Some(StartupNote { status, warnings });
    }
    if changed.is_empty() {
        return None;
    }
    let labels: Vec<&str> = changed
        .iter()
        .map(crate::hook_binary::agent_display_name)
        .collect();
    Some(StartupNote {
        status: format!(
            "Installed or repaired the deck's hooks for {}.",
            and_list(&labels)
        ),
        warnings: Vec::new(),
    })
}

/// What a report says about an agent whose hooks the install could not
/// update, and `why`.
fn could_not_update(agent: &AgentType, why: &[String]) -> String {
    let label = crate::hook_binary::agent_display_name(agent);
    let why = crate::untrusted_text::display_line(
        why.join("; ").trim_end_matches('.'),
        MAX_SHOWN_REASON_BYTES,
    );
    format!("{label}'s hooks could not be updated: {why}.")
}

/// What follows the startup install's failures when the TUI exits.
pub const STARTUP_FIX: &str =
    "Fix what it names, then start the deck again or run `dot-agent-deck hooks refresh`.";

/// The most bytes of a configured path a report repeats.
const MAX_SHOWN_BINARY_BYTES: usize = 512;

/// The most bytes of why an install failed a report repeats.
const MAX_SHOWN_REASON_BYTES: usize = 512;

/// The report for what the agents' configs name after the install:
/// `installed` holds every installed agent's outcome; `is_this` says whether
/// a pinned binary is this copy, and `version` what another one reports, if
/// anything.
pub(crate) fn report(
    deck: &DeckIdentity,
    installed: Vec<Installed>,
    is_this: impl Fn(&str) -> bool,
    version: impl Fn(&str) -> Option<String>,
) -> RefreshReport {
    let text = |text: String| RefreshLine {
        text,
        command: None,
    };
    if installed.is_empty() {
        return RefreshReport {
            ok: true,
            lines: vec![text(NO_AGENTS.to_string())],
        };
    }
    let mut current = Vec::new();
    let mut attention = Vec::new();
    let mut notes = Vec::new();
    for Installed {
        agent,
        failed,
        pins,
    } in installed
    {
        let label = crate::hook_binary::agent_display_name(&agent);
        // A failed install says so even when the config still names this
        // copy: what it names is not what the install meant to write.
        if !failed.is_empty() {
            notes.push(text(could_not_update(&agent, &failed)));
            attention.push(agent);
            continue;
        }
        let Some(pins) = pins else {
            notes.push(text(format!(
                "{label}'s hooks could not be read after the update."
            )));
            attention.push(agent);
            continue;
        };
        let mut binaries: Vec<String> = Vec::new();
        for pin in pins {
            if !binaries.contains(&pin.binary) {
                binaries.push(pin.binary);
            }
        }
        if binaries.is_empty() {
            notes.push(text(format!("{label}'s hooks could not be updated.")));
            attention.push(agent);
            continue;
        }
        let mut settled = true;
        for binary in binaries.iter().filter(|binary| !is_this(binary)) {
            let shown = crate::untrusted_text::display_line(binary, MAX_SHOWN_BINARY_BYTES);
            // Settled only when both releases were read: an answer the probe
            // accepted but that does not parse says nothing about readiness.
            let order =
                |release: &str| crate::hook_binary::compare_releases(&deck.version, release);
            match version(binary) {
                Some(release) if order(&release).is_some_and(|o| o.is_le()) => {
                    notes.push(text(format!(
                        "{label}'s hooks run another copy, {shown} (v{release}), which was left as it is."
                    )));
                }
                Some(release) if order(&release).is_some() => {
                    notes.push(text(format!(
                        "{label}'s hooks still run an older copy, {shown} (v{release})."
                    )));
                    settled = false;
                }
                _ => {
                    notes.push(text(format!(
                        "{label}'s hooks still run another copy, {shown}."
                    )));
                    settled = false;
                }
            }
        }
        if !settled {
            attention.push(agent);
        } else if binaries.iter().any(|binary| is_this(binary)) {
            current.push(label);
        }
    }
    let mut lines = Vec::new();
    if !current.is_empty() {
        lines.push(text(format!(
            "{} hooks now run this copy (v{}). Restart agents that were already running so they use them.",
            and_list(&current),
            crate::hook_binary::release_of_build(&deck.version)
        )));
    }
    lines.extend(notes);
    if !attention.is_empty() {
        let remedy = crate::hook_binary::remedy_for(deck, false, &attention);
        match remedy.command {
            Some(command) => {
                lines.push(text(UPDATE_WITH.to_string()));
                lines.push(RefreshLine {
                    text: command.clone(),
                    command: Some(command),
                });
                lines.push(text(RESTART_AFTER.to_string()));
            }
            None => lines.push(text(remedy.text)),
        }
    }
    RefreshReport {
        ok: attention.is_empty(),
        lines,
    }
}

/// Said when no installed agent's hooks were found.
pub const NO_AGENTS: &str = "No installed agent's hooks were found to update.";
/// The lead-in to the command that pins this copy for the agents left over.
pub const UPDATE_WITH: &str = "To have them run this copy, run:";
/// What follows that command.
pub const RESTART_AFTER: &str = "Then restart the affected agents.";

/// `items` as English: `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const THIS: &str = "/home/u/.local/bin/dot-agent-deck";

    fn deck() -> DeckIdentity {
        DeckIdentity {
            exe: Some(PathBuf::from(THIS)),
            version: "0.48.0".into(),
            self_install_path: Some(THIS.into()),
        }
    }

    fn pins(agent: AgentType, binaries: &[&str]) -> Installed {
        let pins = binaries
            .iter()
            .map(|binary| HookPin {
                agent: agent.clone(),
                config: PathBuf::from("/home/u/.claude/settings.json"),
                binary: binary.to_string(),
            })
            .collect();
        Installed {
            agent,
            failed: Vec::new(),
            pins: Some(pins),
        }
    }

    fn run(read: Vec<Installed>, versions: &[(&str, &str)]) -> RefreshReport {
        report(
            &deck(),
            read,
            |binary| binary == THIS,
            |binary| {
                versions
                    .iter()
                    .find(|(path, _)| *path == binary)
                    .map(|(_, version)| version.to_string())
            },
        )
    }

    /// Scenario: the TUI's startup install changed nothing and nothing failed,
    /// so there is no note; when it changed Claude Code's and Codex's hooks,
    /// the status line names both and nothing is printed on exit.
    #[test]
    fn a_startup_install_that_changed_hooks_names_the_agents() {
        assert_eq!(startup_note(&[], &[]), None);
        assert_eq!(
            startup_note(&[AgentType::ClaudeCode, AgentType::Codex], &[]),
            Some(StartupNote {
                status: "Installed or repaired the deck's hooks for Claude Code and Codex.".into(),
                warnings: Vec::new(),
            })
        );
    }

    /// Scenario: the startup install could not update Claude Code's hooks
    /// because its settings file is not valid JSON, and could not write
    /// OpenCode's plugin. The status line leads with the first reason, counts
    /// the rest, and the exit report names each with the fix; a failure wins
    /// over another agent's change.
    #[test]
    fn a_startup_install_that_failed_says_why_and_what_to_do() {
        let malformed = "/home/u/.claude/settings.json is not valid JSON (left unchanged): \
                         expected `,` or `}` at line 3 column 5";
        let note = startup_note(
            &[AgentType::Codex],
            &[
                (AgentType::ClaudeCode, vec![malformed.to_string()]),
                (AgentType::OpenCode, vec!["/p: denied.".to_string()]),
            ],
        )
        .expect("a failure is always a note");
        let claude = format!("Claude Code's hooks could not be updated: {malformed}.");
        assert_eq!(note.status, format!("{claude} (+1 more, printed on exit)"));
        assert_eq!(
            note.warnings,
            vec![
                claude,
                "OpenCode's hooks could not be updated: /p: denied.".to_string(),
                STARTUP_FIX.to_string(),
            ]
        );
        let one = startup_note(&[], &[(AgentType::Devin, vec!["no\u{1b}[2J".to_string()])])
            .expect("a failure is always a note");
        assert!(!one.status.contains('\u{1b}'), "{:?}", one.status);
        assert!(!one.status.contains("more"), "{:?}", one.status);
    }

    /// Scenario: after the startup install, Claude Code's and Codex's hooks
    /// both name this copy: the report is settled and says so once, with the
    /// restart the running agents need, and no command.
    #[test]
    fn hooks_on_this_copy_are_settled_and_say_to_restart() {
        let report = run(
            vec![
                pins(AgentType::ClaudeCode, &[THIS]),
                pins(AgentType::Codex, &[THIS, THIS]),
            ],
            &[],
        );
        assert!(report.ok);
        assert_eq!(
            report.lines,
            vec![RefreshLine {
                text: "Claude Code and Codex hooks now run this copy (v0.48.0). Restart agents that were already running so they use them.".into(),
                command: None,
            }]
        );
    }

    /// Scenario: the startup install kept another live copy for Claude Code.
    /// One reporting the same release or a newer one is fine and only named;
    /// one reporting an older release, or none, needs this copy's
    /// `hooks install` for that agent, which the report gives as a command.
    #[test]
    fn a_kept_copy_is_named_and_an_unsettled_one_gets_the_command() {
        let other = "/opt/homebrew/bin/dot-agent-deck";
        let same = run(
            vec![pins(AgentType::ClaudeCode, &[other])],
            &[(other, "0.48.0")],
        );
        assert!(same.ok, "{same:?}");
        assert!(same.lines.iter().all(|line| line.command.is_none()));
        assert!(same.lines[0].text.contains(other), "{same:?}");

        for versions in [&[(other, "0.46.0")][..], &[]] {
            let kept = run(
                vec![
                    pins(AgentType::ClaudeCode, &[other]),
                    pins(AgentType::Codex, &[THIS]),
                ],
                versions,
            );
            assert!(!kept.ok, "{kept:?}");
            let commands: Vec<_> = kept
                .lines
                .iter()
                .filter_map(|l| l.command.clone())
                .collect();
            assert_eq!(
                commands,
                vec![format!("{THIS} hooks install --agent claude-code")],
                "{kept:?}"
            );
            assert!(kept.lines[0].text.starts_with("Codex hooks now run"));
            assert!(kept.render().iter().any(|line| line.contains(other)));
            assert_eq!(kept.lines.last().unwrap().text, RESTART_AFTER);
        }
    }

    /// Scenario: the kept copy's `--version` answer passed the probe's check,
    /// but its release cannot be compared with this one's. Only a comparison
    /// that read both releases settles it: the same release and a newer one
    /// are left as they are, and the unreadable one needs this copy's
    /// `hooks install`, as a copy that answered no version does.
    #[test]
    fn a_kept_copy_is_settled_only_by_a_release_both_sides_parse() {
        let other = "/opt/homebrew/bin/dot-agent-deck";
        assert_eq!(
            crate::version::parse_version_output("dot-agent-deck 0.46.bad").as_deref(),
            Some("0.46.bad"),
            "the probe accepts it"
        );
        for settled in ["0.48.0", "0.49.0"] {
            let report = run(
                vec![pins(AgentType::ClaudeCode, &[other])],
                &[(other, settled)],
            );
            assert!(report.ok, "{settled}: {report:?}");
            assert!(report.lines[0].text.contains("which was left as it is"));
        }
        let report = run(
            vec![pins(AgentType::ClaudeCode, &[other])],
            &[(other, "0.46.bad")],
        );
        assert!(!report.ok, "{report:?}");
        assert!(
            report
                .lines
                .iter()
                .all(|line| !line.text.contains("left as it is")),
            "{report:?}"
        );
        assert_eq!(
            report.lines[0].text,
            format!("Claude Code's hooks still run another copy, {other}.")
        );
        assert!(
            report
                .lines
                .iter()
                .any(|line| line.command
                    == Some(format!("{THIS} hooks install --agent claude-code"))),
            "{report:?}"
        );
    }

    /// Scenario: an installed agent whose config names no deck binary after the
    /// install, and a run with no installed agent at all.
    #[test]
    fn nothing_pinned_needs_attention_and_no_agent_is_said() {
        let report = run(vec![pins(AgentType::ClaudeCode, &[])], &[]);
        assert!(!report.ok);
        assert_eq!(
            report.lines[0].text,
            "Claude Code's hooks could not be updated."
        );
        assert!(report.lines.iter().any(|line| line.command.is_some()));

        let none = run(Vec::new(), &[]);
        assert!(none.ok);
        assert_eq!(none.render(), vec![NO_AGENTS.to_string()]);
    }

    /// Scenario: Claude Code's install failed, once on settings it could not
    /// read and once on a write that failed while its old settings still name
    /// this copy, and beside a Codex whose install succeeded. Claude Code's
    /// failure is said with why, it is never reported as running this copy,
    /// the report is not ok, and its `hooks install` is the command; Codex is
    /// reported as updated and given no command.
    #[test]
    fn a_failed_install_needs_attention_whatever_the_config_names() {
        let failed = |pins: Option<Vec<HookPin>>| Installed {
            agent: AgentType::ClaudeCode,
            failed: vec!["left /home/u/.claude/settings.json unchanged: bad JSON.".into()],
            pins,
        };
        let on_this = pins(AgentType::ClaudeCode, &[THIS]).pins;
        for claude in [failed(None), failed(on_this)] {
            let alone = run(vec![claude.clone()], &[]);
            assert!(!alone.ok, "{alone:?}");
            assert_eq!(
                alone.lines[0].text,
                "Claude Code's hooks could not be updated: left /home/u/.claude/settings.json unchanged: bad JSON."
            );
            assert!(
                alone
                    .lines
                    .iter()
                    .all(|line| !line.text.contains("now run")),
                "{alone:?}"
            );

            let mixed = run(vec![claude, pins(AgentType::Codex, &[THIS])], &[]);
            assert!(!mixed.ok, "{mixed:?}");
            assert!(
                mixed.lines[0]
                    .text
                    .starts_with("Codex hooks now run this copy"),
                "{mixed:?}"
            );
            assert!(
                mixed.lines[1]
                    .text
                    .starts_with("Claude Code's hooks could not")
            );
            let commands: Vec<_> = mixed
                .lines
                .iter()
                .filter_map(|line| line.command.clone())
                .collect();
            assert_eq!(
                commands,
                vec![format!("{THIS} hooks install --agent claude-code")]
            );
        }
    }

    /// Scenario: the install ran without a complaint, but the config could
    /// not be read back: the report cannot say what the hooks run, so the
    /// agent needs attention rather than being left out.
    #[test]
    fn an_unreadable_config_after_the_install_needs_attention() {
        let report = run(
            vec![Installed {
                agent: AgentType::ClaudeCode,
                failed: Vec::new(),
                pins: None,
            }],
            &[],
        );
        assert!(!report.ok, "{report:?}");
        assert_eq!(
            report.lines[0].text,
            "Claude Code's hooks could not be read after the update."
        );
        assert!(report.lines.iter().any(|line| line.command.is_some()));
    }

    /// Scenario: the absent/failed split an agent's install reports.
    #[test]
    fn an_install_attempt_tells_absent_from_failed() {
        assert_eq!(InstallAttempt::of::<()>(None), InstallAttempt::Absent);
        assert_eq!(
            InstallAttempt::of(Some(Ok(()))),
            InstallAttempt::Ran { failed: Vec::new() }
        );
        assert_eq!(
            InstallAttempt::of::<()>(Some(Err("x".into()))),
            InstallAttempt::Ran {
                failed: vec!["x".into()]
            }
        );
    }

    /// The JSON a newer copy prints decodes in an older client that knows
    /// fewer fields, and the other way round.
    #[test]
    fn the_report_round_trips_and_tolerates_unknown_fields() {
        let report = run(vec![pins(AgentType::ClaudeCode, &[THIS])], &[]);
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<RefreshReport>(&json).unwrap(),
            report
        );
        let newer = r#"{"ok":false,"lines":[{"text":"x","command":"y","why":1}],"agents":[]}"#;
        let read: RefreshReport = serde_json::from_str(newer).unwrap();
        assert!(!read.ok);
        assert_eq!(read.lines[0].command.as_deref(), Some("y"));
    }
}
