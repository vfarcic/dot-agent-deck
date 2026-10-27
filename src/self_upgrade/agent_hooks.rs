//! What an upgrade of the CLI does about the agents' deck hooks (PRD #1666
//! part 3, R-3), in the same words in the TUI, the CLI and the desktop app.
//!
//! The agents' hooks name one dot-agent-deck. Until the deck next starts and
//! runs its automatic install, an upgrade can leave them on the older copy,
//! and hooks from before the deck's messages never ask for them. So once an
//! upgrade has actually installed a CLI, the client runs `hooks refresh` from
//! that installed copy ([`crate::hooks_refresh`]), which is the startup's own
//! guarded install, and shows its report. A download the user has still to
//! install, a `brew upgrade` that did not reach the release and an app-only
//! upgrade refresh nothing; where the refresh cannot run or answer, the client
//! says so and gives the command to run.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use super::{CLI_BINARY, CopyKind, Host, Outcome, PlanAction, PlanLine, UpgradePlan};
use crate::hooks_refresh::RefreshReport;

/// How long the installed copy's `hooks refresh` may take: the startup
/// installs, which probe each agent's own CLI, and a `--version` probe of
/// each other binary the hooks name.
pub const HOOKS_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// The most lines of a refresh's report a client shows.
const MAX_REPORT_LINES: usize = 24;

/// The most bytes of one report line a client shows.
const MAX_REPORT_TEXT_BYTES: usize = 1024;

/// The CLI that `outcome` of `plan` installed, when it installed one: the file
/// the install wrote, or the Homebrew binary that reported the new release. A
/// staged download, a `brew upgrade` that did not reach the release and the
/// desktop app's own upgrades installed no CLI.
pub fn installed_cli(plan: &UpgradePlan, outcome: &Outcome) -> Option<PathBuf> {
    if plan.installation.copy != CopyKind::Cli {
        return None;
    }
    match (&plan.action, outcome) {
        (PlanAction::ReplaceBinary { .. }, Outcome::Replaced { path, .. }) => Some(path.clone()),
        (PlanAction::StagedInstall { target, .. }, Outcome::Installed { .. }) => {
            Some(target.clone())
        }
        (PlanAction::BrewUpgrade { brew, .. }, Outcome::BrewUpgraded { .. }) => {
            brew.parent().map(|bin| bin.join(CLI_BINARY))
        }
        _ => None,
    }
}

/// What to tell the user about the agents' hooks after `outcome` of `plan`,
/// running the installed CLI's `hooks refresh` through `host` when the
/// upgrade installed one. Empty when the upgrade changed no CLI.
pub fn after_upgrade(host: &dyn Host, plan: &UpgradePlan, outcome: &Outcome) -> Vec<PlanLine> {
    if plan.installation.copy != CopyKind::Cli {
        return Vec::new();
    }
    if let (Outcome::Staged { .. }, PlanAction::StagedInstall { target, .. }) =
        (outcome, &plan.action)
    {
        return match refresh_command(target) {
            Some(command) => vec![
                PlanLine::Text(NOT_INSTALLED.to_string()),
                PlanLine::Command(command),
            ],
            None => vec![PlanLine::Text(NOT_INSTALLED_UNSHOWN.to_string())],
        };
    }
    let Some(cli) = installed_cli(plan, outcome) else {
        return Vec::new();
    };
    let answer = host.run_within(
        &cli,
        &[
            OsStr::new("hooks"),
            OsStr::new("refresh"),
            OsStr::new("--json"),
        ],
        HOOKS_REFRESH_TIMEOUT,
    );
    let report = match answer {
        Ok(output) => serde_json::from_str::<RefreshReport>(output.stdout.trim())
            .map_err(|_| UNREADABLE.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => Err(TIMED_OUT.to_string()),
        Err(error) => Err(format!("it could not be run ({error})")),
    };
    match report.map(|report| shown(&report)) {
        Ok(lines) if !lines.is_empty() => lines,
        Ok(_) => unchecked(&cli, UNREADABLE),
        Err(reason) => unchecked(&cli, &reason),
    }
}

/// Said after a download the user has still to install.
pub const NOT_INSTALLED: &str =
    "Agent hooks were not updated. Once it is installed, update them with:";
/// Said instead of [`NOT_INSTALLED`] and its command when the install's path
/// cannot be shown faithfully in one.
pub const NOT_INSTALLED_UNSHOWN: &str = "Agent hooks were not updated. Once it is installed, run `hooks refresh` with the installed dot-agent-deck to update them; its path cannot be shown safely here.";
/// The reason given when the installed copy's answer could not be read.
const UNREADABLE: &str = "its answer could not be read";
/// The reason given when the installed copy did not answer in time.
const TIMED_OUT: &str = "it did not answer in time";

/// `binary hooks refresh`, for this platform's default shell, or `None` when
/// `binary` cannot be shown faithfully in a command.
fn refresh_command(binary: &std::path::Path) -> Option<String> {
    command_for(binary, "hooks refresh")
}

/// `binary <args>` for this platform's default shell, through the upgrade's
/// own check of a path a shown command names
/// ([`crate::self_upgrade::plan::shown_path`]): `None` for a path that is not
/// UTF-8, too long, or carries a character a reader would not see, so no
/// command ever names another file than `binary`.
fn command_for(binary: &std::path::Path, args: &str) -> Option<String> {
    let shown = crate::self_upgrade::plan::shown_path(binary)?;
    Some(format!(
        "{} {args}",
        crate::hook_binary::shell_program(shown)
    ))
}

/// The installed copy's report as a client shows it: each text made one safe
/// line, and a command kept only when it is exactly what it would copy.
fn shown(report: &RefreshReport) -> Vec<PlanLine> {
    report
        .lines
        .iter()
        .take(MAX_REPORT_LINES)
        .filter_map(|line| match &line.command {
            Some(command) => (!command.is_empty()
                && command.len() <= crate::hook_binary::MAX_NOTICE_COMMAND_BYTES
                && crate::untrusted_text::display_line(
                    command,
                    crate::hook_binary::MAX_NOTICE_COMMAND_BYTES,
                ) == *command)
                .then(|| PlanLine::Command(command.clone())),
            None => Some(PlanLine::Text(crate::untrusted_text::display_line(
                &line.text,
                MAX_REPORT_TEXT_BYTES,
            ))),
        })
        .collect()
}

/// What the client says when the installed copy's hooks could not be checked:
/// why, and the command that updates Claude Code's hooks, the agent whose
/// older hooks leave its messages waiting.
/// When `cli` cannot be shown faithfully in a command, the same is said in
/// words, with no command.
fn unchecked(cli: &std::path::Path, reason: &str) -> Vec<PlanLine> {
    let lead = format!("The CLI was upgraded, but its agent hooks could not be checked: {reason}.");
    match command_for(cli, "hooks install --agent claude-code") {
        Some(command) => vec![
            PlanLine::Text(format!("{lead} To update Claude Code's hooks, run:")),
            PlanLine::Command(command),
            PlanLine::Text(crate::hooks_refresh::RESTART_AFTER.to_string()),
        ],
        None => vec![
            PlanLine::Text(format!(
                "{lead} To update Claude Code's hooks, run `hooks install --agent claude-code` with the upgraded dot-agent-deck; its path cannot be shown safely here."
            )),
            PlanLine::Text(crate::hooks_refresh::RESTART_AFTER.to_string()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_upgrade::detect::Tools;
    use crate::self_upgrade::{
        CommandOutput, HomebrewFormula, InstallMethod, Installation, Platform, Provenance,
        ProvenanceCheck,
    };
    use std::path::Path;
    use std::sync::Mutex;

    const CLI: &str = "/home/u/.local/bin/dot-agent-deck";

    /// A machine whose only command is the installed CLI's `hooks refresh`,
    /// answering `answer`, and which records what it was asked to run.
    struct Fake {
        answer: std::io::Result<CommandOutput>,
        ran: Mutex<Vec<String>>,
    }

    impl Fake {
        fn answering(stdout: &str, success: bool) -> Self {
            Self {
                answer: Ok(CommandOutput {
                    success,
                    code: Some(if success { 0 } else { 1 }),
                    stdout: stdout.into(),
                    stderr: String::new(),
                }),
                ran: Mutex::new(Vec::new()),
            }
        }
        fn failing(kind: std::io::ErrorKind) -> Self {
            Self {
                answer: Err(std::io::Error::new(kind, "boom")),
                ran: Mutex::new(Vec::new()),
            }
        }
        fn ran(&self) -> Vec<String> {
            self.ran.lock().unwrap().clone()
        }
    }

    impl Host for Fake {
        fn run_within(
            &self,
            program: &Path,
            args: &[&OsStr],
            _timeout: Duration,
        ) -> std::io::Result<CommandOutput> {
            let line = std::iter::once(program.as_os_str())
                .chain(args.iter().copied())
                .map(|part| part.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.ran.lock().unwrap().push(line);
            match &self.answer {
                Ok(output) => Ok(output.clone()),
                Err(error) => Err(std::io::Error::new(error.kind(), error.to_string())),
            }
        }
        fn find_program(&self, _: &str) -> Option<PathBuf> {
            None
        }
        fn is_executable(&self, _: &Path) -> bool {
            false
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn canonicalize(&self, _: &Path) -> Option<PathBuf> {
            None
        }
        fn dir_writable(&self, _: &Path) -> bool {
            false
        }
        fn home(&self) -> Option<PathBuf> {
            None
        }
        fn is_wsl(&self) -> bool {
            false
        }
    }

    fn plan(copy: CopyKind, action: PlanAction) -> UpgradePlan {
        UpgradePlan {
            installation: Installation {
                copy,
                executable: PathBuf::from(CLI),
                version: "0.47.0".into(),
                platform: Some(Platform::LinuxAmd64),
                method: InstallMethod::DownloadedWritable {
                    binary: PathBuf::from(CLI),
                },
                tools: Tools::default(),
            },
            latest: "0.48.0".into(),
            action,
            provenance: ProvenanceCheck::Unavailable {
                reason: "no gh".into(),
            },
            stable_switch: None,
        }
    }

    fn replace() -> UpgradePlan {
        plan(
            CopyKind::Cli,
            PlanAction::ReplaceBinary {
                target: PathBuf::from(CLI),
                asset: "dot-agent-deck-linux-amd64".into(),
            },
        )
    }

    fn replaced() -> Outcome {
        Outcome::Replaced {
            path: PathBuf::from(CLI),
            version: "0.48.0".into(),
            provenance: Provenance::Skipped {
                reason: "no gh".into(),
            },
            synced: true,
        }
    }

    fn texts(lines: &[PlanLine]) -> String {
        plan_text(lines).join("\n")
    }

    fn plan_text(lines: &[PlanLine]) -> Vec<String> {
        crate::self_upgrade::plan::render_lines(lines)
    }

    fn commands(lines: &[PlanLine]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|line| match line {
                PlanLine::Command(command) => Some(command.clone()),
                PlanLine::Text(_) => None,
            })
            .collect()
    }

    /// Scenario: the CLI was replaced in place, and its `hooks refresh` reports
    /// every agent's hooks on it. The client ran that refresh from the file the
    /// upgrade wrote and shows the report's own words, with no command.
    #[test]
    fn a_replaced_cli_refreshes_the_hooks_and_shows_the_report() {
        let report = r#"{"ok":true,"lines":[{"text":"Claude Code hooks now run this copy (v0.48.0). Restart agents that were already running so they use them."}]}"#;
        let host = Fake::answering(report, true);
        let lines = after_upgrade(&host, &replace(), &replaced());
        assert_eq!(host.ran(), vec![format!("{CLI} hooks refresh --json")]);
        assert_eq!(
            texts(&lines),
            "Claude Code hooks now run this copy (v0.48.0). Restart agents that were already running so they use them."
        );
        assert!(commands(&lines).is_empty());
    }

    /// Scenario: the refresh left another live install in place and says what
    /// to run; its command comes through as a command, so a client can copy
    /// it, and a command that is not one safe line is dropped, not altered.
    #[test]
    fn a_refresh_that_needs_attention_keeps_its_command() {
        let command = format!("{CLI} hooks install --agent claude-code");
        let report = serde_json::json!({"ok":false,"lines":[
            {"text":"Claude Code's hooks still run another copy, /opt/x."},
            {"text":"To have them run this copy, run:"},
            {"text":command,"command":command},
            {"text":"bad","command":"rm -rf /\u{1b}[2J"},
            {"text":"Then restart the affected agents."}
        ]});
        let host = Fake::answering(&report.to_string(), false);
        let lines = after_upgrade(&host, &replace(), &replaced());
        assert_eq!(commands(&lines), vec![command]);
        assert!(texts(&lines).contains("/opt/x"));
        assert!(!texts(&lines).contains("rm -rf"));
    }

    /// Scenario: the installed copy cannot be run, does not answer in time, or
    /// answers something that is not a report (a build without `hooks refresh`
    /// prints its usage). Each time the client says the hooks could not be
    /// checked, why, and gives the command that updates Claude Code's hooks.
    #[test]
    fn a_refresh_that_cannot_answer_names_the_command() {
        for (host, why) in [
            (
                Fake::failing(std::io::ErrorKind::NotFound),
                "it could not be run",
            ),
            (Fake::failing(std::io::ErrorKind::TimedOut), TIMED_OUT),
            (
                Fake::answering("error: unrecognized subcommand 'refresh'", false),
                UNREADABLE,
            ),
            (
                Fake::answering(r#"{"ok":true,"lines":[]}"#, true),
                UNREADABLE,
            ),
        ] {
            let lines = after_upgrade(&host, &replace(), &replaced());
            let text = texts(&lines);
            assert!(text.contains("could not be checked"), "{text}");
            assert!(text.contains(why), "{why}: {text}");
            assert_eq!(
                commands(&lines),
                vec![format!("{CLI} hooks install --agent claude-code")]
            );
        }
    }

    /// Scenario: a download still to be installed refreshes nothing and says
    /// so, with the command to run once it is installed; Homebrew that did not
    /// reach the release, and an upgrade of the desktop app, say nothing about
    /// hooks and run nothing.
    #[test]
    fn nothing_installed_refreshes_nothing() {
        let target = "/usr/local/bin/dot-agent-deck";
        let staged_plan = plan(
            CopyKind::Cli,
            PlanAction::StagedInstall {
                target: PathBuf::from(target),
                asset: "dot-agent-deck-linux-amd64".into(),
                pkexec: None,
            },
        );
        let staged = Outcome::Staged {
            path: PathBuf::from("/stage/dot-agent-deck"),
            command: Some("sudo install …".into()),
            version: "0.48.0".into(),
            provenance: Provenance::Skipped {
                reason: "no gh".into(),
            },
        };
        let host = Fake::answering("{}", true);
        let lines = after_upgrade(&host, &staged_plan, &staged);
        assert_eq!(
            lines,
            vec![
                PlanLine::Text(NOT_INSTALLED.into()),
                PlanLine::Command(format!("{target} hooks refresh")),
            ]
        );

        let brew_plan = plan(
            CopyKind::Cli,
            PlanAction::BrewUpgrade {
                brew: PathBuf::from("/opt/homebrew/bin/brew"),
                formula: HomebrewFormula::Stable,
            },
        );
        let behind = Outcome::BrewNotUpgraded {
            formula: "dot-agent-deck",
            reported: Some("0.47.0".into()),
            offered: "0.48.0".into(),
            refresh_failed: None,
        };
        assert!(after_upgrade(&host, &brew_plan, &behind).is_empty());

        let app = plan(
            CopyKind::Desktop,
            PlanAction::ReplaceBinary {
                target: PathBuf::from(CLI),
                asset: "x".into(),
            },
        );
        assert!(after_upgrade(&host, &app, &replaced()).is_empty());
        assert!(host.ran().is_empty(), "{:?}", host.ran());
    }

    /// The paths [`crate::self_upgrade::plan`] will not put in a shown
    /// command (`plan_017`, `plan_018`): control and formatting characters, a
    /// backslash, a path past the shown length, and one that is not UTF-8.
    fn unshowable() -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            "/home/u/state\n/x",
            "/home/u/st\u{1b}[2Jate/x",
            "/home/u/\u{202E}etats/x",
            "/home/u/sta\u{200B}te/x",
            "/home/u/state\u{FEFF}/x",
            "/home/u/state\u{0085}/x",
            "/home/u/back\\slash/x",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        paths.push(PathBuf::from(format!(
            "/home/u/{}",
            "x".repeat(crate::self_upgrade::plan::MAX_SHOWN_PATH_CHARS)
        )));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            paths.push(PathBuf::from(OsStr::from_bytes(b"/home/u/st\xffate/x")));
        }
        paths
    }

    /// Scenario: the CLI is at a path no command can show faithfully. After a
    /// download still to be installed, and after an upgrade whose refresh did
    /// not answer, the client gives the recovery in words, naming the
    /// subcommand to run from the installed copy, and shows no command and no
    /// part of the path, rather than a command that names another file.
    #[test]
    fn an_unshowable_path_gets_words_not_a_command() {
        for bad in unshowable() {
            let staged_plan = plan(
                CopyKind::Cli,
                PlanAction::StagedInstall {
                    target: bad.clone(),
                    asset: "dot-agent-deck-linux-amd64".into(),
                    pkexec: None,
                },
            );
            let staged = Outcome::Staged {
                path: PathBuf::from("/stage/dot-agent-deck"),
                command: None,
                version: "0.48.0".into(),
                provenance: Provenance::Skipped {
                    reason: "no gh".into(),
                },
            };
            let host = Fake::failing(std::io::ErrorKind::NotFound);
            let lines = after_upgrade(&host, &staged_plan, &staged);
            assert!(commands(&lines).is_empty(), "{bad:?}: {lines:?}");
            let text = texts(&lines);
            assert!(text.contains("hooks refresh"), "{bad:?}: {text}");
            assert!(!text.contains("/home/u"), "{bad:?}: {text}");

            let outcome = Outcome::Replaced {
                path: bad.clone(),
                version: "0.48.0".into(),
                provenance: Provenance::Skipped {
                    reason: "no gh".into(),
                },
                synced: true,
            };
            let lines = after_upgrade(&host, &replace(), &outcome);
            assert!(commands(&lines).is_empty(), "{bad:?}: {lines:?}");
            let text = texts(&lines);
            assert!(text.contains("could not be checked"), "{bad:?}: {text}");
            assert!(
                text.contains("hooks install --agent claude-code"),
                "{bad:?}: {text}"
            );
            assert!(!text.contains("/home/u"), "{bad:?}: {text}");
        }
    }

    /// Scenario: the words that replace a command for an unshowable path reach
    /// the user unchanged in the CLI's output and in the TUI's dialog, which
    /// both filter what they show, so neither client shows something the
    /// other does not; the desktop app's side is `self_upgrade_036`.
    #[test]
    fn the_clients_show_an_unshowable_paths_recovery_alike() {
        let host = Fake::failing(std::io::ErrorKind::NotFound);
        for bad in unshowable() {
            let outcome = Outcome::Replaced {
                path: bad.clone(),
                version: "0.48.0".into(),
                provenance: Provenance::Skipped {
                    reason: "no gh".into(),
                },
                synced: true,
            };
            let lines = after_upgrade(&host, &replace(), &outcome);
            let words: Vec<String> = lines
                .iter()
                .map(|line| match line {
                    PlanLine::Text(text) => text.clone(),
                    PlanLine::Command(command) => panic!("{bad:?}: a command {command:?}"),
                })
                .collect();
            let mut printed = Vec::new();
            crate::self_upgrade::cli::say_items(&mut printed, &lines);
            assert_eq!(
                String::from_utf8(printed).unwrap(),
                format!("{}\n", words.join("\n")),
                "{bad:?}"
            );
            let result = crate::upgrade_dialog::RunResult::from_outcome_and_hooks(
                &replace(),
                &outcome,
                lines.clone(),
                false,
            );
            assert!(result.lines.ends_with(&lines), "{bad:?}");
            for word in &words {
                assert_eq!(
                    crate::untrusted_text::strip_control_and_bidi(word, false),
                    *word,
                    "the TUI draws it as it is"
                );
            }
        }
    }

    /// Scenario: a `brew upgrade` that reached the release refreshes from the
    /// Homebrew binary beside the `brew` that ran, the one that reported it.
    #[test]
    fn homebrew_refreshes_from_the_binary_that_reported_the_release() {
        let brew_plan = plan(
            CopyKind::Cli,
            PlanAction::BrewUpgrade {
                brew: PathBuf::from("/opt/homebrew/bin/brew"),
                formula: HomebrewFormula::Stable,
            },
        );
        let upgraded = Outcome::BrewUpgraded {
            formula: "dot-agent-deck",
            reported: Some("0.48.0".into()),
        };
        assert_eq!(
            installed_cli(&brew_plan, &upgraded),
            Some(PathBuf::from("/opt/homebrew/bin/dot-agent-deck"))
        );
    }
}
