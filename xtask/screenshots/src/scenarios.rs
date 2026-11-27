//! The named docs-screenshot scenarios, and where each client's half lives.
//!
//! One registry for both clients, so "which screenshots exist" has one answer
//! and `cargo docs-screenshots --list` can print it. The capture code itself
//! lives with the client it drives:
//!
//! - a TUI half is an `#[ignore]`d test named `docs_screenshot_<name>` (with
//!   `-` spelled `_`) in [`TUI_CAPTURE_FILE`], which drives the real binary in
//!   the L2 PTY harness and writes `<name>-tui.html`;
//! - a desktop half is a `desktopScenario("<name>", …)` call in
//!   [`DESKTOP_CAPTURE_FILE`], which drives the production web build.
//!
//! The unit tests below read both files and fail when either one and this list
//! disagree, so a scenario cannot be registered without a capture, or captured
//! without being registered.

/// The file holding every scenario's TUI capture, relative to the repo root.
pub const TUI_CAPTURE_FILE: &str = "tests/e2e_docs_screenshots.rs";

/// The file holding every scenario's desktop capture, relative to the repo root.
pub const DESKTOP_CAPTURE_FILE: &str = "desktop/screenshots/desktop.shot.ts";

/// One of the two clients a scenario can be captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Client {
    Tui,
    Desktop,
}

impl Client {
    pub const ALL: [Client; 2] = [Client::Tui, Client::Desktop];

    /// The spelling used on the command line and in file names.
    pub fn as_str(self) -> &'static str {
        match self {
            Client::Tui => "tui",
            Client::Desktop => "desktop",
        }
    }

    pub fn parse(text: &str) -> Option<Client> {
        Client::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

/// One of the two themes a screenshot is taken in (PR #2491): a PR shows
/// every changed screen in both, side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 2] = [Theme::Light, Theme::Dark];

    /// The spelling used on the command line, in file names and in the
    /// Playwright test titles.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    pub fn parse(text: &str) -> Option<Theme> {
        Theme::ALL.into_iter().find(|t| t.as_str() == text)
    }
}

/// How a run names its images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Naming {
    /// `<scenario>-<client>-<theme>.png`, one file per theme: the default, and
    /// what a pull request's screenshots use.
    Paired,
    /// `<scenario>-<client>.png` in one theme: the names the landing page and
    /// `docs/installation.md` already reference (`--single-theme`).
    Single,
}

impl Naming {
    /// The spelling `cargo docs-screenshots` hands the rasterizer.
    pub fn as_str(self) -> &'static str {
        match self {
            Naming::Paired => "paired",
            Naming::Single => "single",
        }
    }
}

/// A named screenshot, captured from one or both clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scenario {
    /// Kebab-case; the images are `<name>-<client>-<theme>.png`, or
    /// `<name>-<client>.png` in a single-theme run.
    pub name: &'static str,
    /// One line for `--list`.
    pub description: &'static str,
    pub clients: &'static [Client],
}

impl Scenario {
    pub fn has(&self, client: Client) -> bool {
        self.clients.contains(&client)
    }

    /// The PNG this scenario produces for `client` in a single-theme run.
    pub fn image_file(&self, client: Client) -> String {
        format!("{}-{}.png", self.name, client.as_str())
    }

    /// The PNG this scenario produces for `client` in `theme`, named as
    /// `naming` says.
    pub fn themed_image_file(&self, client: Client, theme: Theme, naming: Naming) -> String {
        match naming {
            Naming::Single => self.image_file(client),
            Naming::Paired => format!("{}-{}-{}.png", self.name, client.as_str(), theme.as_str()),
        }
    }

    /// The name of the test function holding the TUI capture.
    pub fn tui_test_name(&self) -> String {
        format!("docs_screenshot_{}", self.name.replace('-', "_"))
    }
}

/// Every scenario, in `--list` order.
pub const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "dashboard",
        description: "The agent list with four agents in mixed states — the TUI dashboard (card column beside the focused agent's pane) and the desktop Dashboard.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "dashboard-empty",
        description: "The agent list with no agents running — each client's empty state.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "antigravity-needs-input",
        description: "An Antigravity agent waiting for permission, with its reason beneath the status.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "pull-request-badge",
        description: "An agent with open pull request #1234 awaiting review, on its TUI card and desktop Dashboard row.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "dashboard-fleet",
        description: "The desktop Dashboard with agents grouped under two connected daemons.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "dashboard-filter",
        description: "The desktop Dashboard across two daemons, filtered to working agents, with the filter line and Show all.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards",
        description: "The desktop Dashboard's Cards view as a tree: an orchestrator opened to show its two roles beside a standalone agent, three needing you, every card with its checkbox and its action icons in a tinted group, Go ahead on three and Merge it on one (PRD #2374, issue #2418, PR #2491 review).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-action-hover",
        description: "A card's Merge it icon hovered in the desktop Cards view: its tooltip with the action's label and the exact text it sends (PR #2491 review).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-select",
        description: "The desktop Cards view selecting agents for a batch: the orchestrator root (which selects its two roles) and a standalone agent ticked, and the selection bar at the foot of the Dashboard with its count, Go ahead, Close and Merge it in the action group, and Clear (issue #2399, PR #2491 review).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-select-edit",
        description: "The desktop selection bar with two cards on one daemon ticked: after the action group and set apart from it, the Edit card actions sliders button under the pointer (PR #2491, 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-select-edit-menu",
        description: "The desktop selection bar with a card ticked on each of two daemons: Edit card actions open on its menu naming both daemons (PR #2491, 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-card-tab-terminal",
        description: "An agent's pane opened from its card in the desktop Cards view, on its Terminal tab: the agent at work in its terminal, and beside Terminal the Card tab with its bell, because the agent needs you (PR #2491 decision 2026-11-26).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-card-tab",
        description: "The same agent pane on its Card tab, in the terminal's place: the deck-checked status and pull request, the agent's summary, why it needs you, its question with two answers, and its action icons (PR #2491 decision 2026-11-26).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-card-tab-narrow",
        description: "The same Card tab in a 375-pixel window, scrolled to its Actions with Edit beside the heading (PR #2491 decision 2026-11-26).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch",
        description: "The desktop Cards view's batch confirmation of Go ahead for four selected agents: three included, each with the exact text it gets, and one skipped with its reason (issue #2399).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch-many",
        description: "A batch confirmation over every agent of a large tree: the included agents' first names and +N more, and the skipped agents folded into one line counting them by reason (PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch-texts",
        description: "The same large batch with two agents that worded their own Go ahead: each distinct text quoted with its source mark (catalog, agent says), the catalog text's long list of names folded behind +N more, and the heading counting the agents and the 3 different texts (PR #2491 audit K1).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch-texts-unseen",
        description: "The same confirmation in a 1440 by 500 window, the two texts the agents wrote below the fold: Send disabled with \"Scroll to see all 3 texts\" and Show next text beside it, until each text has been in view (PR #2491 audit K1).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch-many-expanded",
        description: "The same confirmation with the skipped line pressed: the skipped agents listed, grouped by reason (PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-batch-results",
        description: "The same batch after Send: each agent's outcome, two queued for the agent and one failed with the daemon's reason (issue #2399).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-actions-editor",
        description: "The desktop Card actions editor, opened from the daemon's ⋯ menu and titled with its name: the form for a new action, Deploy it, with its text over two lines and its icon grid open on the rocket; the listed actions are below it (PRD #2374 decision 2026-11-24, PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-wrapped",
        description: "The desktop Cards view with two trees open in one row, each in its own box captioned with its root: a dispatcher whose orchestration is opened inside it, and one whose seven agents wrap into two rows, each row hanging from the spine by its own line (issue #2533).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "agent-cards-daemon-menu",
        description: "A daemon's ⋯ menu on the desktop Dashboard, open on Card actions, with the cards right under the top bar (PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "dashboard-more-menu",
        description: "The desktop Dashboard's ⋯ menu, right of Refresh, open on Needs you only, with the count of agents that need you on the ⋯ (PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "dashboard-needs-you-chip",
        description: "Needs you only on in the desktop Dashboard's Table view: the Needs you only chip beside Filter, and only the agents that need you with the agents above them (PR #2491 decision 2026-11-25).",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "new-agent",
        description: "The New Agent form with a project directory selected.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "orchestration",
        description: "An active orchestration with planner and builder roles.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "generate-config",
        description: "The Generate .dot-agent-deck.toml confirmation, Yes and No, opened on one agent: in the TUI with `g` on its card beside its pane, on the desktop from the Generate config button in its pane's header.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "agent-messaging",
        description: "Planner and builder working with one queued inbox message each.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "inbox-attention",
        description: "A Pi card whose first prompt may not have reached the agent, with Deck messages open.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "agent-pane",
        description: "The desktop agent pane over the Dashboard.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "settings-daemons",
        description: "Desktop Daemons settings with one remote configured.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "add-daemon",
        description: "The desktop Add a daemon dialog over the empty Dashboard, opened from its empty-agents note, with a host and user typed in.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "upgrade-all",
        description: "The desktop Upgrade all daemons dialog over the upgrade fleet, stopped at the busy remote daemon's own restart question, with the other remote waiting its turn below and Keep current daemon and Restart now.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "daemon-upgrade",
        description: "The desktop Upgrade dialog for a remote daemon on an older release, stopped at the restart question that names the agents a restart would stop, with Keep current daemon and Restart now.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "self-upgrade",
        description: "The upgrade dialog open at its first question, one release behind v0.47.0: in the TUI, the footer's update badge naming the CLI and this CLI's plan as a downloaded binary, saying the running daemon is not restarted; on the desktop, the Dashboard with the app's newer-release banner and the rail's upgrade button, and the plan for the app from a .dmg.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "local-cli-upgrade",
        description: "The desktop Dashboard with the app current and a Homebrew CLI one release behind v0.47.0: no app-wide banner, Upgrade on the Local daemon section, and the dialog it opens with the CLI's plan, saying the daemon is not restarted, at its question.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "settings-voice",
        description: "Desktop Voice settings.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "settings-voice-reading",
        description: "Desktop Voice settings scrolled to reading: the Reading switch with the sentence saying what it sends where, and the Speech source picker.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "voice-typing-mode",
        description: "The desktop agent pane with voice typing mode on: the Typing to marker at the window's top edge, the voice row's reminder and its Stop typing button.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "voice-choice",
        description: "The desktop dashboard with the numbered voice choice open: a dialog centred over the screen offering two agents as a numbered list, with its countdown and Cancel, and the voice row below saying what was heard.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "voice-question",
        description: "The desktop agent pane with a Codex command approval waiting and \"go ahead\" heard: the voice row shows the answer, naming the option (Yes, proceed) and the command, with its five-second countdown and Cancel.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "voice-numbers",
        description: "The desktop agent dashboard with voice on: a number before each agent row, one sequence across the daemons, ready to be said or pressed.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "voice-pages",
        description: "The desktop New agent dialog with voice on over a long directory: the directories fill the dialog as numbered columns, one page at a time, with the page marker beside the Directory heading.",
        clients: &[Client::Desktop],
    },
    Scenario {
        name: "schedules",
        description: "The TUI Schedules manager with one configured task.",
        clients: &[Client::Tui],
    },
    Scenario {
        name: "help",
        description: "The TUI keyboard shortcut help overlay.",
        clients: &[Client::Tui],
    },
    Scenario {
        name: "host-metrics",
        description: "The host of a deck: the TUI's Host of this deck overlay and the desktop Dashboard's Host button opened, for one fixed sample.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "restart-notice",
        description: "The deck restarted while the client was attached: the notice naming the two agents the restart stopped, api-fix and docs-review, in the TUI's footer row and the desktop's corner notice, over a deck now running none; the desktop's preview daemon also lists interrupted work, so its notice adds that line with a Show button (PRD #1790).",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "interrupted-work",
        description: "What a deck restart left behind: the Interrupted work list, opened with `I` in the TUI and from the dashboard banner in the desktop app, listing a dispatched unit, an orchestration and a plain agent under Other agents (PRD #1790).",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "project-trust",
        description: "Review project commands for the orchestration `team`, changed since it was approved: `coder` changed, `tester` new, `reviewer` removed — opened from New Agent in both clients (PRD #1885).",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "star-prompt",
        description: "The star reminder at startup, on a launch it shows at, in both clients: the TUI's star prompt and the desktop app's Enjoying dot-agent-deck? dialog over an empty dashboard (PRD #2174).",
        clients: &[Client::Tui, Client::Desktop],
    },
];

/// Look a scenario up by name.
pub fn find(name: &str) -> Option<&'static Scenario> {
    SCENARIOS.iter().find(|s| s.name == name)
}

/// Every `docs_screenshot_<x>` test function named in `source`, as `<x>`.
pub fn tui_captures_in(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("fn docs_screenshot_") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        found.push(name);
    }
    found
}

/// Every `desktopScenario("<x>"` call in `source`, as `<x>`.
pub fn desktop_captures_in(source: &str) -> Vec<String> {
    const CALL: &str = "desktopScenario(\"";
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find(CALL) {
        rest = &rest[at + CALL.len()..];
        if let Some(end) = rest.find('"') {
            found.push(rest[..end].to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn repo_file(relative: &str) -> String {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        std::fs::read_to_string(root.join(relative))
            .unwrap_or_else(|e| panic!("read {relative}: {e}"))
    }

    #[test]
    fn names_are_unique_kebab_case_and_every_scenario_has_a_client() {
        let mut seen = BTreeSet::new();
        for s in SCENARIOS {
            assert!(seen.insert(s.name), "duplicate scenario {}", s.name);
            assert!(!s.clients.is_empty(), "{} has no client", s.name);
            assert!(
                !s.name.is_empty()
                    && s.name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !s.name.starts_with('-')
                    && !s.name.ends_with('-'),
                "{} is not kebab-case",
                s.name
            );
        }
    }

    #[test]
    fn file_and_test_names_follow_the_convention() {
        let s = find("dashboard-empty").unwrap();
        assert_eq!(s.image_file(Client::Tui), "dashboard-empty-tui.png");
        assert_eq!(s.image_file(Client::Desktop), "dashboard-empty-desktop.png");
        assert_eq!(s.tui_test_name(), "docs_screenshot_dashboard_empty");
        assert_eq!(Client::parse("tui"), Some(Client::Tui));
        assert_eq!(Client::parse("gui"), None);
    }

    #[test]
    fn a_paired_run_names_one_file_per_theme_and_a_single_run_keeps_the_old_name() {
        let s = find("dashboard").unwrap();
        assert_eq!(
            s.themed_image_file(Client::Tui, Theme::Light, Naming::Paired),
            "dashboard-tui-light.png"
        );
        assert_eq!(
            s.themed_image_file(Client::Desktop, Theme::Dark, Naming::Paired),
            "dashboard-desktop-dark.png"
        );
        for theme in Theme::ALL {
            assert_eq!(
                s.themed_image_file(Client::Desktop, theme, Naming::Single),
                "dashboard-desktop.png"
            );
        }
        assert_eq!(Theme::parse("light"), Some(Theme::Light));
        assert_eq!(Theme::parse("dark"), Some(Theme::Dark));
        assert_eq!(Theme::parse("system"), None);
    }

    #[test]
    fn the_scanners_find_what_they_are_meant_to() {
        let rust =
            "#[test]\n#[ignore = \"x\"]\nfn docs_screenshot_dashboard_empty() {}\nfn other() {}\n";
        assert_eq!(tui_captures_in(rust), vec!["dashboard_empty"]);
        let ts = "desktopScenario(\"dashboard\", async () => {});\n  desktopScenario(\"b-c\", f);";
        assert_eq!(desktop_captures_in(ts), vec!["dashboard", "b-c"]);
    }

    /// The registry and the TUI capture file name the same scenarios.
    #[test]
    fn every_tui_scenario_has_exactly_one_capture_and_vice_versa() {
        let captured: Vec<String> = tui_captures_in(&repo_file(TUI_CAPTURE_FILE));
        let captured_set: BTreeSet<_> = captured.iter().cloned().collect();
        assert_eq!(
            captured.len(),
            captured_set.len(),
            "a TUI capture is defined twice"
        );
        let registered: BTreeSet<String> = SCENARIOS
            .iter()
            .filter(|s| s.has(Client::Tui))
            .map(|s| s.name.replace('-', "_"))
            .collect();
        assert_eq!(
            registered, captured_set,
            "SCENARIOS and {TUI_CAPTURE_FILE} disagree"
        );
    }

    /// The registry and the desktop capture file name the same scenarios.
    #[test]
    fn every_desktop_scenario_has_exactly_one_capture_and_vice_versa() {
        let captured = desktop_captures_in(&repo_file(DESKTOP_CAPTURE_FILE));
        let captured_set: BTreeSet<_> = captured.iter().cloned().collect();
        assert_eq!(
            captured.len(),
            captured_set.len(),
            "a desktop capture is defined twice"
        );
        let registered: BTreeSet<String> = SCENARIOS
            .iter()
            .filter(|s| s.has(Client::Desktop))
            .map(|s| s.name.to_string())
            .collect();
        assert_eq!(
            registered, captured_set,
            "SCENARIOS and {DESKTOP_CAPTURE_FILE} disagree"
        );
    }
}
