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

/// Run every agent's startup install, then report what each installed agent's
/// hooks run. An agent whose config is missing or could not be read is left
/// out: its startup install refused or had nothing to do.
pub fn refresh() -> RefreshReport {
    use crate::agent_registry::ALL;
    for spec in ALL {
        if let Some(install) = spec.startup_auto_install {
            install();
        }
    }
    let deck = DeckIdentity::current(crate::hook_binary::process_resolution());
    let read = ALL
        .iter()
        .filter_map(|spec| {
            let read = spec.configured_pins?;
            read().map(|pins| (spec.agent_type.clone(), pins))
        })
        .collect();
    report(
        &deck,
        read,
        |binary| crate::hook_binary::names_this_deck(&deck, binary),
        |binary| {
            std::path::Path::new(binary)
                .is_absolute()
                .then(|| crate::hook_binary::probe_version(std::path::Path::new(binary)).ok())
                .flatten()
        },
    )
}

/// The most bytes of a configured path a report repeats.
const MAX_SHOWN_BINARY_BYTES: usize = 512;

/// The report for what the agents' configs name after the install: `read`
/// holds, per installed agent, its pins; `is_this` says whether a pinned
/// binary is this copy, and `version` what another one reports, if anything.
pub(crate) fn report(
    deck: &DeckIdentity,
    read: Vec<(AgentType, Vec<HookPin>)>,
    is_this: impl Fn(&str) -> bool,
    version: impl Fn(&str) -> Option<String>,
) -> RefreshReport {
    let text = |text: String| RefreshLine {
        text,
        command: None,
    };
    if read.is_empty() {
        return RefreshReport {
            ok: true,
            lines: vec![text(NO_AGENTS.to_string())],
        };
    }
    let mut current = Vec::new();
    let mut attention = Vec::new();
    let mut notes = Vec::new();
    for (agent, pins) in read {
        let label = crate::hook_binary::agent_display_name(&agent);
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
            match version(binary) {
                Some(release) if !crate::hook_binary::is_newer_release(&deck.version, &release) => {
                    notes.push(text(format!(
                        "{label}'s hooks run another copy, {shown} (v{release}), which was left as it is."
                    )));
                }
                Some(release) => {
                    notes.push(text(format!(
                        "{label}'s hooks still run an older copy, {shown} (v{release})."
                    )));
                    settled = false;
                }
                None => {
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

    fn pins(agent: AgentType, binaries: &[&str]) -> (AgentType, Vec<HookPin>) {
        let pins = binaries
            .iter()
            .map(|binary| HookPin {
                agent: agent.clone(),
                config: PathBuf::from("/home/u/.claude/settings.json"),
                binary: binary.to_string(),
            })
            .collect();
        (agent, pins)
    }

    fn run(read: Vec<(AgentType, Vec<HookPin>)>, versions: &[(&str, &str)]) -> RefreshReport {
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
