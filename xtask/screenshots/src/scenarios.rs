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

/// A named screenshot, captured from one or both clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scenario {
    /// Kebab-case; the image is `docs/img/<name>-<client>.png`.
    pub name: &'static str,
    /// One line for `--list`.
    pub description: &'static str,
    pub clients: &'static [Client],
}

impl Scenario {
    pub fn has(&self, client: Client) -> bool {
        self.clients.contains(&client)
    }

    /// The PNG this scenario produces for `client`.
    pub fn image_file(&self, client: Client) -> String {
        format!("{}-{}.png", self.name, client.as_str())
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
        description: "The agent list with four agents in mixed states — the TUI dashboard and the desktop agent overview.",
        clients: &[Client::Tui, Client::Desktop],
    },
    Scenario {
        name: "dashboard-empty",
        description: "The agent list with no agents running — each client's empty state.",
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
