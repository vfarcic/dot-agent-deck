//! L1 tests for the TUI's upgrade dialog (issue #1635): the dialog drawn from
//! the self-upgrade core's own plans, its confirm-one-copy-at-a-time state
//! machine, the footer badge's text and the key that opens it.
//!
//! The spawned-binary path — the badge appearing, the key opening the dialog,
//! a confirmed upgrade replacing a file — is `tests/e2e_self_upgrade.rs`.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::keybindings::{Action as KbAction, KeybindingConfig};
use dot_agent_deck::self_upgrade::detect::Tools;
use dot_agent_deck::self_upgrade::{
    CopyKind, HomebrewFormula, InstallMethod, Installation, Outcome, PlanOptions, Platform,
    Provenance, ProvenanceCheck, UPDATE_RECHECK_INTERVAL, UpgradeError, UpgradePlan, plan,
};
use dot_agent_deck::ui::{
    render_help_overlay_with_bindings_to_buffer, render_upgrade_dialog_to_buffer,
};
use dot_agent_deck::upgrade_dialog::{
    Effect, Phase, RunResult, UpgradeCheck, UpgradeChoice, UpgradeDialog, badge_text,
    recheck_interval,
};
use spec::spec;

const LATEST: &str = "0.47.0";
const CURRENT: &str = "0.46.0";
const CLI_EXE: &str = "/home/u/.local/bin/dot-agent-deck";

fn buffer_to_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    let mut out = String::with_capacity((area.width as usize + 1) * area.height as usize);
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// The dialog's text with its border and the line breaks of its word wrap
/// removed, so a sentence the dialog wrapped still reads as one.
fn flat(text: &str) -> String {
    text.lines()
        .map(|line| line.trim().trim_matches('│').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn render(dialog: &UpgradeDialog) -> String {
    buffer_to_text(&render_upgrade_dialog_to_buffer(dialog, 120, 40))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn options() -> PlanOptions {
    PlanOptions {
        staging_root: PathBuf::from("/home/u/.local/state/dot-agent-deck/upgrade"),
        can_prompt_for_privilege: false,
        provenance: ProvenanceCheck::Unavailable {
            reason: dot_agent_deck::self_upgrade::verify::GH_NOT_INSTALLED.into(),
        },
    }
}

fn installation(copy: CopyKind, exe: &str, version: &str, method: InstallMethod) -> Installation {
    Installation {
        copy,
        executable: PathBuf::from(exe),
        version: version.into(),
        platform: Some(Platform::LinuxAmd64),
        method,
        tools: Tools::default(),
    }
}

fn plan_for(installation: &Installation) -> UpgradePlan {
    plan::plan(installation, &LATEST.into(), &options())
}

/// The CLI downloaded into a directory the user can write: replaced in place.
fn writable_cli() -> UpgradePlan {
    plan_for(&installation(
        CopyKind::Cli,
        CLI_EXE,
        CURRENT,
        InstallMethod::DownloadedWritable {
            binary: PathBuf::from(CLI_EXE),
        },
    ))
}

/// The desktop app installed from the `.deb`, with no graphical prompt.
fn deb_desktop() -> UpgradePlan {
    plan_for(&installation(
        CopyKind::Desktop,
        "/usr/bin/dot-agent-deck",
        CURRENT,
        InstallMethod::DesktopDeb,
    ))
}

fn nix_cli() -> UpgradePlan {
    plan_for(&installation(
        CopyKind::Cli,
        "/nix/store/abc-dot-agent-deck-0.46.0/bin/dot-agent-deck",
        CURRENT,
        InstallMethod::Nix,
    ))
}

/// Installed with Homebrew, but `brew` itself was not found: the plan is the
/// command to run.
fn brew_without_brew() -> UpgradePlan {
    plan_for(&installation(
        CopyKind::Cli,
        "/opt/homebrew/Cellar/dot-agent-deck/0.46.0/bin/dot-agent-deck",
        CURRENT,
        InstallMethod::Homebrew {
            formula: HomebrewFormula::Stable,
            prefix: PathBuf::from("/opt/homebrew"),
        },
    ))
}

fn non_writable_cli() -> UpgradePlan {
    plan_for(&installation(
        CopyKind::Cli,
        "/usr/local/bin/dot-agent-deck",
        CURRENT,
        InstallMethod::DownloadedNonWritable {
            binary: PathBuf::from("/usr/local/bin/dot-agent-deck"),
        },
    ))
}

fn skipped() -> Provenance {
    Provenance::Skipped {
        reason: dot_agent_deck::self_upgrade::verify::GH_NOT_INSTALLED.into(),
    }
}

/// Scenario: Open the upgrade dialog on two plans the core made — this TUI's CLI, downloaded into a folder the user can write, and the desktop app installed from its `.deb` — and draw it. The dialog must show both plans in the core's own words, ask about the CLI first, and have Cancel selected so that Enter alone changes nothing.
#[spec("upgrade/upgrade-dialog/001")]
#[test]
fn upgrade_dialog_001_in_place_plan_with_other_copy_defaults_to_cancel() {
    let cli = writable_cli();
    let desktop = deb_desktop();
    let dialog = UpgradeDialog::new(vec![cli.clone(), desktop.clone()]);

    assert_eq!(
        dialog.phase(),
        Phase::Confirm(0),
        "the TUI's own copy is asked about first"
    );
    assert_eq!(
        dialog.selected(),
        UpgradeChoice::Cancel,
        "the confirmation defaults to No"
    );

    let text = render(&dialog);
    let flat = flat(&text);
    assert!(flat.contains(&format!("Upgrade to v{LATEST}")), "{text}");
    // Every line of both plans, as the core wrote them.
    for plan in [&cli, &desktop] {
        for line in plan.lines() {
            assert!(
                flat.contains(line.trim()),
                "missing plan line {line:?}\n{text}"
            );
        }
    }
    assert!(
        flat.contains("Build provenance will NOT be checked"),
        "the plan must say before confirming that provenance is not checked\n{text}"
    );
    let question = cli
        .confirm_question()
        .expect("an in-place plan is actionable");
    assert_eq!(question, format!("Upgrade dot-agent-deck to v{LATEST}?"));
    assert!(flat.contains(&question), "{text}");
    assert!(
        text.contains("> Cancel"),
        "Cancel carries the selection cursor\n{text}"
    );
    assert!(text.contains("  Upgrade"), "{text}");
    assert!(
        !flat.contains("Upgrade Agent Deck (desktop app) to"),
        "one copy is asked about at a time\n{text}"
    );
    insta::assert_snapshot!(text);
}

/// Scenario: Open the upgrade dialog on a CLI installed with Nix and draw it, then press Enter and `y`. The dialog must say, in the core's words, that the copy is not changed from here and what to run instead, offer only Close, and never start an upgrade.
#[spec("upgrade/upgrade-dialog/002")]
#[test]
fn upgrade_dialog_002_notify_only_nix_offers_only_close() {
    let nix = nix_cli();
    assert!(!nix.is_actionable());
    let mut dialog = UpgradeDialog::new(vec![nix.clone()]);
    assert_eq!(dialog.phase(), Phase::Done);
    assert_eq!(dialog.selected(), UpgradeChoice::Close);

    let text = render(&dialog);
    let flat = flat(&text);
    for line in nix.lines() {
        assert!(
            flat.contains(line.trim()),
            "missing plan line {line:?}\n{text}"
        );
    }
    assert!(flat.contains("nix profile upgrade"), "{text}");
    assert!(text.contains("> Close"), "{text}");
    assert!(
        !flat.contains("Cancel"),
        "a notify-only plan offers only Close\n{text}"
    );
    assert!(!flat.contains("Upgrade dot-agent-deck to"), "{text}");

    assert_eq!(dialog.handle_key(key(KeyCode::Char('y'))), Effect::None);
    assert_eq!(
        dialog.choose(UpgradeChoice::Upgrade),
        Effect::None,
        "nothing to upgrade"
    );
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
    insta::assert_snapshot!(text);
}

/// Scenario: Open the upgrade dialog on a Homebrew CLI whose `brew` could not be found, and draw it. The dialog must show the exact `brew upgrade` command to run, offer only Close, and close on Escape without starting anything.
#[spec("upgrade/upgrade-dialog/003")]
#[test]
fn upgrade_dialog_003_show_command_plan_offers_only_close() {
    let brew = brew_without_brew();
    assert!(!brew.is_actionable());
    let mut dialog = UpgradeDialog::new(vec![brew.clone()]);
    assert_eq!(dialog.phase(), Phase::Done);

    let text = render(&dialog);
    let flat = flat(&text);
    for line in brew.lines() {
        assert!(
            flat.contains(line.trim()),
            "missing plan line {line:?}\n{text}"
        );
    }
    assert!(text.contains("brew upgrade dot-agent-deck"), "{text}");
    assert!(text.contains("> Close"), "{text}");
    assert!(!flat.contains("Cancel"), "{text}");
    assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Effect::Close);
    insta::assert_snapshot!(text);
}

/// Scenario: Open the upgrade dialog on a CLI in a folder the user cannot write, choose Upgrade, and finish the run with the core's "staged" outcome. While it runs the dialog says Upgrading… and Escape does not close it; afterwards it shows the core's install command, which checks the checksum again before `sudo`, and offers Close.
#[spec("upgrade/upgrade-dialog/004")]
#[test]
fn upgrade_dialog_004_non_writable_dir_shows_the_install_command_after_the_check() {
    let staged = non_writable_cli();
    assert!(
        staged.is_actionable(),
        "a non-writable folder still downloads and checks"
    );
    let mut dialog = UpgradeDialog::new(vec![staged.clone()]);
    assert_eq!(dialog.phase(), Phase::Confirm(0));

    assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::Run(0));
    assert_eq!(dialog.phase(), Phase::Running(0));
    assert!(
        flat(&render(&dialog)).contains("Upgrading…"),
        "{}",
        render(&dialog)
    );
    assert_eq!(
        dialog.handle_key(key(KeyCode::Esc)),
        Effect::None,
        "running cannot be dismissed"
    );
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::None);

    let command = "echo 'abc  /home/u/.local/state/dot-agent-deck/upgrade/x/dot-agent-deck-linux-amd64' | sha256sum -c - && sudo install -m 0755 /home/u/.local/state/dot-agent-deck/upgrade/x/dot-agent-deck-linux-amd64 /usr/local/bin/dot-agent-deck";
    let outcome = Outcome::Staged {
        path: PathBuf::from(
            "/home/u/.local/state/dot-agent-deck/upgrade/x/dot-agent-deck-linux-amd64",
        ),
        command: Some(command.into()),
        version: LATEST.into(),
        provenance: skipped(),
    };
    let result = RunResult::from_outcome(&staged, &outcome, true);
    assert!(result.ok);
    dialog.finish(0, result);
    assert_eq!(dialog.phase(), Phase::Done);

    let text = render(&dialog);
    let flat = flat(&text);
    for line in outcome.lines() {
        assert!(
            flat.contains(line.trim()),
            "missing outcome line {line:?}\n{text}"
        );
    }
    assert!(flat.contains(command), "the command is shown whole\n{text}");
    assert!(text.contains("> Close"), "{text}");
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
}

/// Scenario: Walk the dialog's confirmation through every key: Escape and Enter on the default Cancel close it with nothing run; Down then Enter upgrades this TUI's copy; the result says the upgrade happened and to restart the TUI; the desktop app is then offered behind its own confirmation, defaulting to Cancel, and Cancel there skips it rather than closing; a failure is shown in the core's words.
#[spec("upgrade/upgrade-dialog/005")]
#[test]
fn upgrade_dialog_005_one_copy_at_a_time_and_cancel_skips_after_a_run() {
    let cli = writable_cli();
    let desktop = deb_desktop();
    let plans = vec![cli.clone(), desktop.clone()];

    // Before anything ran, Cancel and Escape close having done nothing.
    let mut dialog = UpgradeDialog::new(plans.clone());
    assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Effect::Close);
    let mut dialog = UpgradeDialog::new(plans.clone());
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);
    let mut dialog = UpgradeDialog::new(plans.clone());
    assert_eq!(dialog.choose(UpgradeChoice::Cancel), Effect::Close);

    // Down selects Upgrade; Up goes back to Cancel.
    let mut dialog = UpgradeDialog::new(plans.clone());
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.selected(), UpgradeChoice::Upgrade);
    dialog.handle_key(key(KeyCode::Up));
    assert_eq!(dialog.selected(), UpgradeChoice::Cancel);
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Run(0));
    assert_eq!(dialog.phase(), Phase::Running(0));

    let outcome = Outcome::Replaced {
        path: PathBuf::from(CLI_EXE),
        version: LATEST.into(),
        provenance: skipped(),
    };
    let result = RunResult::from_outcome(&cli, &outcome, true);
    assert!(result.ok);
    let restart = outcome
        .tui_restart_line(LATEST)
        .expect("replacing the TUI's own binary needs a restart");
    assert!(result.lines.contains(&restart), "{:?}", result.lines);
    for line in outcome.items() {
        assert!(result.lines.contains(&line), "{:?}", result.lines);
    }
    // The desktop app, upgraded from here, is not the TUI the user restarts.
    let other = RunResult::from_outcome(
        &desktop,
        &Outcome::Installed {
            version: LATEST.into(),
            provenance: skipped(),
        },
        false,
    );
    assert!(
        other.lines.iter().all(|line| *line != restart),
        "{:?}",
        other.lines
    );

    dialog.finish(0, result);
    assert_eq!(
        dialog.phase(),
        Phase::Confirm(1),
        "then the other copy, on its own confirm"
    );
    assert_eq!(
        dialog.selected(),
        UpgradeChoice::Cancel,
        "which defaults to No again"
    );
    let text = render(&dialog);
    let flat_text = flat(&text);
    assert!(
        flat_text.contains(&format!("Upgraded {CLI_EXE} to v{LATEST}.")),
        "{text}"
    );
    assert!(flat_text.contains(restart.render().trim()), "{text}");
    assert!(
        flat_text.contains(&format!("Upgrade Agent Deck (desktop app) to v{LATEST}?")),
        "{text}"
    );

    // After a run, Cancel skips this copy instead of closing.
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::None);
    assert_eq!(dialog.phase(), Phase::Done);
    assert!(render(&dialog).contains("> Close"));
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);

    // A failure is the core's error and what to do instead.
    let error = UpgradeError::ChecksumMismatch {
        asset: "dot-agent-deck-linux-amd64".into(),
        manifest: "checksums.txt".into(),
        expected: "a".repeat(64),
        actual: "b".repeat(64),
    };
    let failed = RunResult::from_error(&error);
    assert!(!failed.ok);
    assert_eq!(
        failed.lines.first().map(|line| line.render()),
        Some(error.to_string())
    );
    let mut dialog = UpgradeDialog::new(vec![cli.clone()]);
    assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::Run(0));
    dialog.finish(0, failed);
    assert_eq!(dialog.phase(), Phase::Done);
    assert!(
        flat(&render(&dialog)).contains("Nothing was changed."),
        "{}",
        render(&dialog)
    );
}

/// Scenario: Build the badge from the core's plans and resolve the key that opens the dialog. The badge reads like the desktop app's notice — the headline of the first copy that is behind, this TUI's first — and names the key; `u` opens the dialog by default, can be rebound in `keybindings.toml` without a conflict, and the help overlay lists it; the TUI re-checks on the desktop app's interval.
#[spec("upgrade/upgrade-dialog/006")]
#[test]
fn upgrade_dialog_006_badge_names_a_rebindable_key_and_rechecks_like_the_desktop() {
    let behind = UpgradeCheck {
        plans: vec![writable_cli(), deb_desktop()],
    };
    let notice = behind.notice().expect("a copy is behind");
    assert_eq!(notice, writable_cli().headline());
    assert_eq!(
        notice,
        format!("dot-agent-deck: update available: v{LATEST} (current: v{CURRENT})")
    );

    // This TUI is current but the desktop app is behind: the badge names it.
    let mut current_cli = installation(
        CopyKind::Cli,
        CLI_EXE,
        LATEST,
        InstallMethod::DownloadedWritable {
            binary: PathBuf::from(CLI_EXE),
        },
    );
    current_cli.version = LATEST.into();
    let only_desktop = UpgradeCheck {
        plans: vec![plan_for(&current_cli), deb_desktop()],
    };
    assert_eq!(only_desktop.notice(), Some(deb_desktop().headline()));
    let up_to_date = UpgradeCheck {
        plans: vec![plan_for(&current_cli)],
    };
    assert_eq!(
        up_to_date.notice(),
        None,
        "nothing is shown while every copy is current"
    );

    let badge = badge_text(&notice, "u");
    assert!(badge.contains(&notice), "{badge}");
    assert!(badge.contains("u to upgrade"), "{badge}");

    let defaults = KeybindingConfig::default();
    assert_eq!(defaults.notation(KbAction::OpenUpgrade), "u");
    assert_eq!(
        defaults.action_for(&key(KeyCode::Char('u'))),
        Some(KbAction::OpenUpgrade)
    );
    let (rebound, warnings) =
        KeybindingConfig::from_toml_str("[dashboard]\nopen_upgrade = \"Alt+u\"\n").unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(rebound.notation(KbAction::OpenUpgrade), "Alt+u");
    assert_eq!(rebound.action_for(&key(KeyCode::Char('u'))), None);

    let help = buffer_to_text(&render_help_overlay_with_bindings_to_buffer(
        &defaults, 120, 60,
    ));
    assert!(
        help.lines()
            .any(|line| line.contains(" u ") && line.contains("Upgrade")),
        "the help overlay lists the upgrade key\n{help}"
    );

    assert_eq!(recheck_interval(), UPDATE_RECHECK_INTERVAL);
}
