//! L1 tests for the TUI's upgrade dialog (issue #1635): the dialog drawn from
//! the self-upgrade core's own plans, its confirm-one-copy-at-a-time state
//! machine, the footer badge's text and the key that opens it.
//!
//! The spawned-binary path — the badge appearing, the key opening the dialog,
//! a confirmed upgrade replacing a file — is `tests/e2e_self_upgrade.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use dot_agent_deck::keybindings::{Action as KbAction, KeybindingConfig};
use dot_agent_deck::self_upgrade::detect::Tools;
use dot_agent_deck::self_upgrade::plan::MAX_SHOWN_PATH_CHARS;
use dot_agent_deck::self_upgrade::{
    CopyKind, HomebrewFormula, InstallMethod, Installation, Outcome, PlanOptions, Platform,
    Provenance, ProvenanceCheck, UPDATE_RECHECK_INTERVAL, UpgradeError, UpgradePlan, plan,
};
use dot_agent_deck::ui::{
    render_help_overlay_with_bindings_to_buffer, render_upgrade_dialog_to_buffer,
};
use dot_agent_deck::upgrade_dialog::{
    Effect, Phase, RunResult, UpgradeCheck, UpgradeChoice, UpgradeDialog, badge, badge_text,
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

/// Draw `dialog` at 120×40. Drawing records what reached the screen, which is
/// what lets the user choose Upgrade, so the dialog is taken mutably.
fn render(dialog: &mut UpgradeDialog) -> String {
    render_at(dialog, 120, 40)
}

fn render_at(dialog: &mut UpgradeDialog, width: u16, height: u16) -> String {
    buffer_to_text(&render_upgrade_dialog_to_buffer(dialog, width, height))
}

/// The rows inside the dialog's border, each trimmed of its padding.
fn dialog_rows(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let start = line.find('│')? + '│'.len_utf8();
            let end = line.rfind('│')?;
            (end >= start).then(|| line[start..end].trim().to_string())
        })
        .collect()
}

/// Page through `dialog` at `width`×`height`, from the top to the bottom, and
/// return every body row that reached the screen, in order, with whether the
/// "more above" and "more below" markers were ever shown. `footer` names the
/// first footer row after the body (the question, or `> Close`).
fn page_through(
    dialog: &mut UpgradeDialog,
    width: u16,
    height: u16,
    footer: &str,
) -> (Vec<String>, bool, bool) {
    let mut saw_up = false;
    let mut saw_down = false;
    // To the top first: the dialog may open part way down.
    for _ in 0..100 {
        let text = render_at(dialog, width, height);
        let rows = dialog_rows(&text);
        if !rows[0].contains("↑ more") {
            break;
        }
        saw_up = true;
        dialog.handle_key(key(KeyCode::PageUp));
    }
    let mut body = BTreeMap::new();
    for _ in 0..100 {
        let text = render_at(dialog, width, height);
        let rows = dialog_rows(&text);
        saw_up |= rows[0].contains("↑ more");
        let below = rows
            .iter()
            .position(|row| row.starts_with(footer))
            .unwrap_or_else(|| panic!("no {footer:?} row\n{text}"))
            - 1;
        let more = rows[below].contains("↓ more");
        saw_down |= more;
        for (n, row) in rows[1..below].iter().enumerate() {
            body.insert(dialog.scroll_offset() + n, row.clone());
        }
        if !more {
            break;
        }
        dialog.handle_key(key(KeyCode::PageDown));
    }
    (body.into_values().collect(), saw_up, saw_down)
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
    let mut dialog = UpgradeDialog::new(vec![cli.clone(), desktop.clone()]);

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

    let text = render(&mut dialog);
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

    let text = render(&mut dialog);
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

    let text = render(&mut dialog);
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

    render(&mut dialog);
    assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::Run(0));
    assert_eq!(dialog.phase(), Phase::Running(0));
    assert!(
        flat(&render(&mut dialog)).contains("Upgrading…"),
        "{}",
        render(&mut dialog)
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

    let text = render(&mut dialog);
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
    render(&mut dialog);
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
        synced: true,
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
    let text = render(&mut dialog);
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
    assert!(render(&mut dialog).contains("> Close"));
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
    render(&mut dialog);
    assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::Run(0));
    dialog.finish(0, failed);
    assert_eq!(dialog.phase(), Phase::Done);
    assert!(
        flat(&render(&mut dialog)).contains("Nothing was changed."),
        "{}",
        render(&mut dialog)
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

/// Scenario: At 80×24, upgrade a CLI in a folder the user cannot write whose staging path is long but still short enough to be shown, and finish with the core's "staged" outcome. The install command no longer fits in the dialog, so the dialog marks that there is more above and below, and paging with PageUp and PageDown reaches every row of it, its `sha256sum -c -` check included; nothing is cut off.
#[spec("upgrade/upgrade-dialog/007")]
#[test]
fn upgrade_dialog_007_a_long_command_is_reached_by_scrolling_never_cut() {
    let staging = format!(
        "/home/u/.local/state/dot-agent-deck/upgrade/{}",
        "d".repeat(400)
    );
    assert!(staging.chars().count() < MAX_SHOWN_PATH_CHARS);
    let staged_file = PathBuf::from(format!("{staging}/dot-agent-deck-linux-amd64"));
    let command = plan::install_binary_command(
        Some(Platform::LinuxAmd64),
        &staged_file,
        Path::new("/usr/local/bin/dot-agent-deck"),
        &"a".repeat(64),
    )
    .expect("a path under the limit is shown as a command");
    assert!(command.contains("sha256sum -c -"), "{command}");
    let staged = non_writable_cli();
    let outcome = Outcome::Staged {
        path: staged_file,
        command: Some(command.clone()),
        version: LATEST.into(),
        provenance: skipped(),
    };

    let mut dialog = UpgradeDialog::new(vec![staged.clone()]);
    render_at(&mut dialog, 80, 24);
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Run(0));
    dialog.finish(0, RunResult::from_outcome(&staged, &outcome, true));

    let first = render_at(&mut dialog, 80, 24);
    assert!(
        first.contains("↑ more") || first.contains("↓ more"),
        "the dialog says there is more to read\n{first}"
    );
    assert!(
        flat(&first).contains("PgUp/PgDn"),
        "the hint names the scroll keys\n{first}"
    );

    let (body, saw_up, saw_down) = page_through(&mut dialog, 80, 24, "> Close");
    assert!(saw_up, "a marker shows text above the view");
    assert!(saw_down, "a marker shows text below the view");
    let joined = body.join(" ");
    let squeezed = |text: &str| text.replace(' ', "");
    assert!(
        squeezed(&joined).contains(&squeezed(&command)),
        "every row of the command is reachable\n{joined}"
    );
    assert!(joined.contains("sha256sum -c -"), "{joined}");
    for line in staged.lines().iter().chain(outcome.lines().iter()) {
        let line = line.trim();
        if line.len() < 60 {
            assert!(joined.contains(line), "missing {line:?}\n{joined}");
        }
    }
}

/// Scenario: In a terminal too short for the plan, open the dialog on this TUI's CLI and the desktop app. The view starts at the CLI's plan with a marker that more is below and a hint to read it with PageDown; selecting Upgrade and pressing Enter (or clicking Upgrade) scrolls on instead of upgrading until the whole plan, the provenance line included, has been on screen, and only then upgrades. The desktop app's question then starts at its own plan, which has to be read the same way.
#[spec("upgrade/upgrade-dialog/008")]
#[test]
fn upgrade_dialog_008_upgrade_waits_until_the_whole_plan_was_on_screen() {
    const W: u16 = 80;
    const H: u16 = 15;
    let cli = writable_cli();
    let desktop = deb_desktop();
    let plans = vec![cli.clone(), desktop.clone()];
    let provenance = "Build provenance will NOT be checked";

    let mut dialog = UpgradeDialog::new(plans.clone());
    let first = render_at(&mut dialog, W, H);
    let rows = dialog_rows(&first);
    assert!(
        rows[1].starts_with("dot-agent-deck: update available"),
        "the view starts at this TUI's plan\n{first}"
    );
    assert!(
        !flat(&first).contains(provenance),
        "the terminal is too short for the whole plan\n{first}"
    );
    assert!(first.contains("↓ more"), "{first}");
    assert!(
        flat(&first).contains("PageDown to read the whole plan"),
        "the hint says why Upgrade waits\n{first}"
    );

    // A click on Upgrade is held the same way.
    let mut clicked = dialog.clone();
    assert_eq!(clicked.choose(UpgradeChoice::Upgrade), Effect::None);
    assert_eq!(clicked.phase(), Phase::Confirm(0));

    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.selected(), UpgradeChoice::Upgrade);
    let mut held = 0;
    let mut saw_provenance = false;
    let effect = loop {
        match dialog.handle_key(key(KeyCode::Enter)) {
            Effect::None => {
                held += 1;
                assert!(held < 30, "Upgrade never became available");
                assert_eq!(dialog.phase(), Phase::Confirm(0));
                saw_provenance |= flat(&render_at(&mut dialog, W, H)).contains(provenance);
            }
            effect => break effect,
        }
    };
    assert!(held > 0, "Enter scrolled on before it upgraded");
    assert!(
        saw_provenance,
        "the provenance line was on screen before the upgrade"
    );
    assert_eq!(effect, Effect::Run(0));

    let outcome = Outcome::Replaced {
        path: PathBuf::from(CLI_EXE),
        version: LATEST.into(),
        provenance: skipped(),
        synced: true,
    };
    dialog.finish(0, RunResult::from_outcome(&cli, &outcome, true));
    assert_eq!(dialog.phase(), Phase::Confirm(1));
    let text = render_at(&mut dialog, W, H);
    let rows = dialog_rows(&text);
    assert!(
        rows[1].starts_with("Agent Deck (desktop app): update available"),
        "the desktop app's question starts at its own plan\n{text}"
    );
    assert!(text.contains("↑ more"), "{text}");
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(
        dialog.handle_key(key(KeyCode::Enter)),
        Effect::None,
        "the desktop app's plan has to be read too\n{text}"
    );
}

/// Scenario: Upgrade this TUI's own copy so the new build is on disk (the core's "replaced" outcome), while the desktop app is still behind. The badge then names only the desktop app, and the dialog opened again shows this copy's result and asks only about the desktop app. With nothing else behind, the badge says to restart the TUI in the core's words instead. A "staged" outcome, which still leaves a command to run, keeps offering the copy, and so does an upgrade of the desktop app.
#[spec("upgrade/upgrade-dialog/009")]
#[test]
fn upgrade_dialog_009_an_installed_copy_is_not_offered_again_until_restart() {
    let cli = writable_cli();
    let desktop = deb_desktop();
    let replaced_outcome = Outcome::Replaced {
        path: PathBuf::from(CLI_EXE),
        version: LATEST.into(),
        provenance: skipped(),
        synced: true,
    };
    let restart = replaced_outcome
        .tui_restart_line(LATEST)
        .expect("a replaced TUI is restarted");
    let replaced = RunResult::from_outcome(&cli, &replaced_outcome, true);
    assert_eq!(replaced.tui_restart, Some(restart.clone()));

    let both = UpgradeCheck {
        plans: vec![cli.clone(), desktop.clone()],
    };
    let shown = badge(Some(&both), Some(&replaced), "u").expect("the desktop app is behind");
    assert!(shown.text.contains(&desktop.headline()), "{}", shown.text);
    assert!(!shown.text.contains(&cli.headline()), "{}", shown.text);
    assert!(shown.offers_upgrade);

    let mut dialog = UpgradeDialog::new(both.plans.clone()).with_installed(0, replaced.clone());
    assert_eq!(
        dialog.phase(),
        Phase::Confirm(1),
        "only the desktop app is asked about"
    );
    let text = render(&mut dialog);
    let flat_text = flat(&text);
    assert!(
        flat_text.contains(&format!("Upgraded {CLI_EXE} to v{LATEST}.")),
        "{text}"
    );
    assert!(
        !flat_text.contains(&format!("Upgrade dot-agent-deck to v{LATEST}?")),
        "{text}"
    );
    assert!(
        flat_text.contains("close, changing nothing"),
        "nothing has run in this dialog yet\n{text}"
    );

    // Nothing else is behind: the restart hint takes the badge's place.
    let only_cli = UpgradeCheck {
        plans: vec![cli.clone()],
    };
    let shown = badge(Some(&only_cli), Some(&replaced), "u").expect("a restart hint");
    assert_eq!(shown.text.trim(), restart.render().trim());
    assert!(!shown.offers_upgrade);
    let mut dialog = UpgradeDialog::new(only_cli.plans.clone()).with_installed(0, replaced);
    assert_eq!(dialog.phase(), Phase::Done);
    assert!(render(&mut dialog).contains("> Close"));

    // Staged: the user still has to run the command, so the offer stays.
    let staged_plan = non_writable_cli();
    let staged = RunResult::from_outcome(
        &staged_plan,
        &Outcome::Staged {
            path: PathBuf::from("/home/u/.local/state/dot-agent-deck/upgrade/x/a"),
            command: Some("sudo install a b".into()),
            version: LATEST.into(),
            provenance: skipped(),
        },
        true,
    );
    assert_eq!(staged.tui_restart, None);
    let staged_check = UpgradeCheck {
        plans: vec![staged_plan.clone()],
    };
    let shown = badge(Some(&staged_check), Some(&staged), "u").expect("still behind");
    assert!(
        shown.text.contains(&staged_plan.headline()),
        "{}",
        shown.text
    );
    assert!(shown.offers_upgrade);
    assert_eq!(
        UpgradeDialog::new(staged_check.plans.clone())
            .with_installed(0, staged)
            .phase(),
        Phase::Confirm(0)
    );

    // The desktop app's upgrade is not this TUI's restart.
    let other = RunResult::from_outcome(
        &desktop,
        &Outcome::Installed {
            version: LATEST.into(),
            provenance: skipped(),
        },
        false,
    );
    assert_eq!(other.tui_restart, None);
    assert_eq!(badge(None, None, "u"), None);
}

/// Scenario: A release whose version carries a terminal escape and a bidi override is shown in the badge, the dialog's title and its question. Neither character reaches the screen: they are dropped the same way the plan's own lines are.
#[spec("upgrade/upgrade-dialog/010")]
#[test]
fn upgrade_dialog_010_escapes_in_a_version_never_reach_the_terminal() {
    let hostile = "0.47.0\u{1b}]0;owned\u{7}\u{202e}";
    let mut plan = writable_cli();
    plan.latest = hostile.into();
    assert!(
        plan.headline().contains('\u{1b}'),
        "the core passes the version through; the client filters it"
    );
    let bad = |c: char| c.is_control() || c == '\u{202e}';

    let check = UpgradeCheck {
        plans: vec![plan.clone()],
    };
    let shown = badge(Some(&check), None, "u").expect("behind");
    assert!(!shown.text.chars().any(bad), "{:?}", shown.text);
    assert!(shown.text.contains("v0.47.0"), "{}", shown.text);
    let text = badge_text(&plan.headline(), "u");
    assert!(!text.chars().any(bad), "{text:?}");

    let mut dialog = UpgradeDialog::new(vec![plan]);
    let buffer = render_upgrade_dialog_to_buffer(&mut dialog, 120, 40);
    let screen = buffer_to_text(&buffer);
    assert!(!screen.chars().any(|c| c != '\n' && bad(c)), "{screen:?}");
    let flat_text = flat(&screen);
    assert!(
        flat_text.contains("Upgrade dot-agent-deck to v0.47.0]0;owned?"),
        "{screen}"
    );
    assert!(flat_text.contains("Upgrade to v0.47.0]0;owned"), "{screen}");
}

/// Whether `text` is the dialog's "make the terminal larger" state, asking
/// for `columns`×`rows`.
fn is_resize_state(text: &str, columns: u16, rows: u16) -> bool {
    let flat_text = flat(text);
    flat_text.contains("The terminal is too small to show the upgrade plan.")
        && flat_text.contains(&format!(
            "Make it at least {columns} columns wide and {rows} rows tall."
        ))
        && flat_text.contains("> Close")
        && !flat_text.contains("Upgrade  —")
}

/// Scenario: Open the dialog on this TUI's CLI in a terminal 8 columns wide and 60 rows tall, too narrow for any of the plan to be read. The dialog says the terminal is too small and what size it needs, and offers only Close; paging through it, selecting Upgrade, pressing Enter and clicking Upgrade never start an upgrade, and Enter or Esc closes it.
#[spec("upgrade/upgrade-dialog/011")]
#[test]
fn upgrade_dialog_011_a_too_narrow_terminal_shows_the_resize_state_and_never_upgrades() {
    use dot_agent_deck::upgrade_dialog::MIN_COLUMNS;
    const W: u16 = 8;
    const H: u16 = 60;
    let mut dialog = UpgradeDialog::new(vec![writable_cli()]);
    let first = render_at(&mut dialog, W, H);
    assert!(is_resize_state(&first, MIN_COLUMNS, 13), "{first}");
    assert_eq!(dialog.selected(), UpgradeChoice::Close);

    for _ in 0..50 {
        dialog.handle_key(key(KeyCode::PageDown));
        render_at(&mut dialog, W, H);
    }
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::None);
    assert_eq!(dialog.phase(), Phase::Confirm(0));
    let mut again = dialog.clone();
    assert_eq!(again.handle_key(key(KeyCode::Enter)), Effect::Close);
    assert_eq!(dialog.handle_key(key(KeyCode::Esc)), Effect::Close);

    // Larger again, the plan has still to be read before Upgrade runs.
    let mut wide = dialog.clone();
    render_at(&mut wide, 120, 40);
    let text = render_at(&mut wide, 120, 40);
    assert!(!flat(&text).contains("too small"), "{text}");
    wide.handle_key(key(KeyCode::Down));
    assert_eq!(wide.selected(), UpgradeChoice::Upgrade);
    assert_eq!(wide.handle_key(key(KeyCode::Enter)), Effect::Run(0));
}

/// Scenario: Upgrade a CLI in a folder the user cannot write in a terminal exactly as narrow as the dialog allows, and finish with the core's "staged" outcome. Paging through the result reaches every character of the indented install command. One column narrower, the dialog shows the resize state instead.
#[spec("upgrade/upgrade-dialog/012")]
#[test]
fn upgrade_dialog_012_the_narrowest_allowed_width_cuts_no_command_character() {
    use dot_agent_deck::upgrade_dialog::MIN_COLUMNS;
    const H: u16 = 24;
    let staged_file = PathBuf::from(format!(
        "/home/u/.local/state/dot-agent-deck/upgrade/{}/dot-agent-deck-linux-amd64",
        "d".repeat(150)
    ));
    let command = plan::install_binary_command(
        Some(Platform::LinuxAmd64),
        &staged_file,
        Path::new("/usr/local/bin/dot-agent-deck"),
        &"a".repeat(64),
    )
    .expect("a path under the limit is shown as a command");
    let staged = non_writable_cli();
    let outcome = Outcome::Staged {
        path: staged_file,
        command: Some(command.clone()),
        version: LATEST.into(),
        provenance: skipped(),
    };
    let mut dialog = UpgradeDialog::new(vec![staged.clone()]);
    dialog.finish(0, RunResult::from_outcome(&staged, &outcome, true));

    let narrower = render_at(&mut dialog.clone(), MIN_COLUMNS - 1, H);
    assert!(is_resize_state(&narrower, MIN_COLUMNS, 10), "{narrower}");

    let first = render_at(&mut dialog, MIN_COLUMNS, H);
    assert!(!flat(&first).contains("too small"), "{first}");
    let (body, _, _) = page_through(&mut dialog, MIN_COLUMNS, H, "> Close");
    let squeezed = |text: &str| text.replace(' ', "");
    assert!(
        squeezed(&body.join("")).contains(&squeezed(&command)),
        "every character of the command is reachable\n{}",
        body.join("\n")
    );
}

/// Scenario: Open the dialog on this TUI's CLI in a terminal wide enough but too short to show even one row of the plan beside the question and the buttons. The dialog says the terminal is too small and how many rows it needs, offers only Close, and choosing Upgrade or pressing Enter never starts an upgrade.
#[spec("upgrade/upgrade-dialog/013")]
#[test]
fn upgrade_dialog_013_a_too_short_terminal_shows_the_resize_state() {
    use dot_agent_deck::upgrade_dialog::MIN_COLUMNS;
    let mut dialog = UpgradeDialog::new(vec![writable_cli()]);
    let text = render_at(&mut dialog, 120, 12);
    assert!(is_resize_state(&text, MIN_COLUMNS, 13), "{text}");
    dialog.handle_key(key(KeyCode::Down));
    for _ in 0..20 {
        dialog.handle_key(key(KeyCode::PageDown));
        render_at(&mut dialog, 120, 12);
        assert_eq!(dialog.choose(UpgradeChoice::Upgrade), Effect::None);
    }
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Close);

    // One row taller is enough.
    let mut taller = UpgradeDialog::new(vec![writable_cli()]);
    let text = render_at(&mut taller, 120, 13);
    assert!(!flat(&text).contains("too small"), "{text}");
    assert!(text.contains("↓ more"), "{text}");
}

/// The button drawn with the `>` cursor in `text`, if any.
fn drawn_selection(text: &str) -> Option<UpgradeChoice> {
    let rows: Vec<String> = text
        .lines()
        .map(|line| line.trim().trim_matches('│').trim().to_string())
        .filter(|row| row.starts_with("> "))
        .collect();
    assert!(
        rows.len() <= 1,
        "more than one button drawn as selected\n{text}"
    );
    let row = rows.first()?;
    Some(if row.starts_with("> Cancel") {
        UpgradeChoice::Cancel
    } else if row.starts_with("> Upgrade") {
        UpgradeChoice::Upgrade
    } else if row.starts_with("> Close") {
        UpgradeChoice::Close
    } else {
        panic!("unknown selected button {row:?}\n{text}")
    })
}

/// Scenario: At 120×40, read this TUI's CLI plan and select Upgrade, then shrink the terminal to 8×60 so the dialog asks for a larger one, then grow it back and draw it once. The restored dialog shows Cancel selected, so Enter closes rather than upgrades; Upgrade has to be chosen again, and since the plan was already read it then runs.
#[spec("upgrade/upgrade-dialog/014")]
#[test]
fn upgrade_dialog_014_restoring_from_the_resize_state_selects_cancel_and_cannot_run() {
    let mut dialog = UpgradeDialog::new(vec![writable_cli()]);
    let first = render_at(&mut dialog, 120, 40);
    assert!(!flat(&first).contains("too small"), "{first}");
    dialog.handle_key(key(KeyCode::Down));
    assert_eq!(dialog.selected(), UpgradeChoice::Upgrade);
    let selected = render_at(&mut dialog, 120, 40);
    assert_eq!(
        drawn_selection(&selected),
        Some(dialog.selected()),
        "{selected}"
    );

    // Shrink: the dialog asks for a larger terminal, and what it draws as
    // selected is what it reports.
    let shrunk = render_at(&mut dialog, 8, 60);
    assert!(flat(&shrunk).contains("too small"), "{shrunk}");
    assert_eq!(
        drawn_selection(&shrunk),
        Some(UpgradeChoice::Close),
        "{shrunk}"
    );
    assert_eq!(dialog.selected(), UpgradeChoice::Close);

    // Restore, drawn once: Cancel is selected, on screen and in the state.
    let restored = render_at(&mut dialog, 120, 40);
    assert!(!flat(&restored).contains("too small"), "{restored}");
    assert_eq!(
        drawn_selection(&restored),
        Some(UpgradeChoice::Cancel),
        "the restored dialog draws Cancel as selected\n{restored}"
    );
    assert_eq!(dialog.selected(), UpgradeChoice::Cancel);
    let mut entered = dialog.clone();
    assert_ne!(
        entered.handle_key(key(KeyCode::Enter)),
        Effect::Run(0),
        "Enter on the restored dialog never upgrades"
    );

    // The plan was already read, so choosing Upgrade again runs it.
    dialog.handle_key(key(KeyCode::Down));
    let chosen = render_at(&mut dialog, 120, 40);
    assert_eq!(
        drawn_selection(&chosen),
        Some(UpgradeChoice::Upgrade),
        "{chosen}"
    );
    assert_eq!(dialog.handle_key(key(KeyCode::Enter)), Effect::Run(0));
}
