//! Per-branch reverse probes: the stimulus that reaches the arm a branch
//! actually changed, beyond the generic delegate/hook pair.
//!
//! # Why the generic four tells are not enough in reverse
//!
//! The four tells are rule 12's, and they are what every run asserts. But a
//! change to how the daemon logs a hostile id, re-keys a colliding session or
//! discloses a teardown is not exercised by a delegate and two hooks — those
//! flows never reach the changed line. A probe is the extra stimulus that does,
//! with an assertion on what the branch daemon then did. Each is written from
//! `.dot-agent-deck/xver-evidence/probe-spec.md`, adapted where the spec's
//! stimulus would have needed to leave the namespace or the environment
//! allowlist — the stimulus moves, the isolation does not.
//!
//! A probe is selected by the branch name (`agent/dispatch-issue-<n>`) or
//! explicitly with `--probe`, and ONLY runs in the reverse direction: that is
//! the pairing that executes a daemon-side change. A branch no probe is known
//! for gets [`Probe::Generic`] — the four tells and nothing else — and the
//! evidence file says so rather than implying more was measured.

use serde::{Deserialize, Serialize};

use crate::sandbox::{EndpointMode, Sandbox};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum Probe {
    /// The four tells only.
    Generic,
    /// PR #1161 / issue #1109: an old-TUI `Stop` makes the branch daemon log
    /// what the shutdown frame is destroying, before it exits.
    TeardownInventory,
    /// PR #1168 / issue #1031: a late `SessionStart` from the old hook CLI
    /// makes the branch daemon submit a parked delegate pointer exactly once.
    LateSessionStart,
    /// PR #1169 / issue #1082: a newline-bearing session id from the old hook
    /// CLI is logged as ONE escaped record, and later events still land.
    LogEscaping,
    /// PR #1179 / issue #1121: whether the old client can find a branch daemon
    /// bound in the post-#1121 per-uid fallback directory at all.
    DiscoveryFallback,
    /// PR #1183 / issue #1182: the branch daemon confirms a delivery the old
    /// hook CLI reports inside Claude's paste envelope.
    PasteEnvelope,
    /// PR #1187 / issue #925: two panes' old hook frames under ONE session id
    /// leave two cards, each with its own status.
    CrossPaneSessionKey,
    /// PR #1188 / issue #1129: old fire-and-forget `work-done` and `dispatch`
    /// still take effect on a branch daemon that now writes an acknowledgement.
    SignalAck,
    /// PR #1190 / issue #1181: an old-CLI dispatch under hostile ambient git
    /// location variables changes only the intended repository.
    GitEnv,
}

impl Probe {
    /// The probe the branch's issue number selects.
    pub fn for_branch(branch: &str) -> Probe {
        let issue = branch
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|n| n.parse::<u32>().ok());
        match issue {
            Some(1109) => Probe::TeardownInventory,
            Some(1031) => Probe::LateSessionStart,
            Some(1082) => Probe::LogEscaping,
            Some(1121) => Probe::DiscoveryFallback,
            Some(1182) => Probe::PasteEnvelope,
            Some(925) => Probe::CrossPaneSessionKey,
            Some(1129) => Probe::SignalAck,
            Some(1181) => Probe::GitEnv,
            _ => Probe::Generic,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Probe::Generic => "generic",
            Probe::TeardownInventory => "teardown-inventory",
            Probe::LateSessionStart => "late-session-start",
            Probe::LogEscaping => "log-escaping",
            Probe::DiscoveryFallback => "discovery-fallback",
            Probe::PasteEnvelope => "paste-envelope",
            Probe::CrossPaneSessionKey => "cross-pane-session-key",
            Probe::SignalAck => "signal-ack",
            Probe::GitEnv => "git-env",
        }
    }

    /// `(PR, issue)` the probe was written for.
    pub fn origin(self) -> Option<(u32, u32)> {
        match self {
            Probe::Generic => None,
            Probe::TeardownInventory => Some((1161, 1109)),
            Probe::LateSessionStart => Some((1168, 1031)),
            Probe::LogEscaping => Some((1169, 1082)),
            Probe::DiscoveryFallback => Some((1179, 1121)),
            Probe::PasteEnvelope => Some((1183, 1182)),
            Probe::CrossPaneSessionKey => Some((1187, 925)),
            Probe::SignalAck => Some((1188, 1129)),
            Probe::GitEnv => Some((1190, 1181)),
        }
    }

    /// One line for the evidence file.
    pub fn describe(self) -> String {
        match (self, self.origin()) {
            (Probe::Generic, _) | (_, None) => {
                "generic — the four tells only; no branch-specific stimulus".to_string()
            }
            (p, Some((pr, issue))) => format!("`{}` — PR #{pr} / issue #{issue}", p.name()),
        }
    }

    /// Refuse a probe whose run configuration cannot reach its changed arm.
    ///
    /// `DiscoveryFallback` is the one with a hard requirement: #1121 moved the
    /// endpoint only in the no-XDG, no-override fallback, so any other
    /// configuration measures an arm it did not touch.
    pub fn check_config(self, mode: EndpointMode, keep_xdg: bool) -> Result<(), String> {
        match self {
            Probe::DiscoveryFallback if mode != EndpointMode::Resolved || keep_xdg => Err(
                "the discovery-fallback probe needs `--endpoint-mode resolved \
                 --unset-xdg-runtime-dir`: #1121 changed only the no-XDG, no-override fallback \
                 arm, and any other configuration resolves an arm it did not touch"
                    .to_string(),
            ),
            Probe::DiscoveryFallback => Ok(()),
            _ if mode != EndpointMode::SandboxSockets => Err(format!(
                "the {} probe is written for `--endpoint-mode sandbox-sockets`",
                self.name()
            )),
            _ => Ok(()),
        }
    }

    /// Roles appended to the orchestration after the fixture's three.
    ///
    /// * `CrossPaneSessionKey` — two more shells, `alpha` and `beta`, so ONE
    ///   session id can arrive from two distinct live pane/agent identities.
    ///   (The fixture's `orchestrator` and `reviewer` are shells too, but the
    ///   generic tells have already driven both.)
    /// * `LateSessionStart` — `lateboot`, a `clear = true` worker whose command's
    ///   basename is `claude`, which is what makes the deck type it as Claude
    ///   Code (`AgentType::from_command`) and so what makes the #1031 recovery
    ///   eligible at all. It is the synthetic stand-in `stub.rs`, by absolute
    ///   path: the daemon replaces its `PATH` with a login shell's, where a bare
    ///   `claude` could resolve to a real one.
    pub fn extra_roles(self, sb: &Sandbox) -> Vec<ExtraRole> {
        let shell = |name: &str| ExtraRole {
            name: name.to_string(),
            command: format!(
                "printf 'XVER_{}_SENTINEL\\n'; exec sh -i",
                name.to_ascii_uppercase()
            ),
            clear: false,
        };
        match self {
            Probe::CrossPaneSessionKey => vec![shell(ROLE_ALPHA), shell(ROLE_BETA)],
            Probe::LateSessionStart => vec![ExtraRole {
                name: ROLE_LATEBOOT.to_string(),
                command: format!("{} --xver-mode=late-boot", sb.stub_claude().display()),
                clear: true,
            }],
            _ => Vec::new(),
        }
    }

    /// Environment entries the probe adds to EVERY process of the run (see
    /// `sandbox::run_env`): names off the base allowlist, each with an exact
    /// value.
    ///
    /// * `LateSessionStart` — the three delegate timers the probe spec names,
    ///   so no unrelated report or buffer interleaves with the recovery, and
    ///   debug logging, which is where the recovery's own trail is written.
    /// * `GitEnv` — #1181's eight location variables, set coherently to the
    ///   decoy repository, for the daemon and everything it spawns. The
    ///   harness's own git commands never see them (`sandbox::sandbox_git`
    ///   starts from an empty environment).
    pub fn extra_env(self, sb: &Sandbox) -> Vec<(String, String)> {
        let kv = |k: &str, v: String| (k.to_string(), v);
        match self {
            Probe::LateSessionStart => vec![
                kv("RUST_LOG", "dot_agent_deck=debug".to_string()),
                kv("DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS", "0".to_string()),
                kv(
                    "DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS",
                    "0".to_string(),
                ),
                kv(
                    "DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS",
                    "0".to_string(),
                ),
            ],
            Probe::GitEnv => {
                let decoy = decoy_repo(sb);
                let git = decoy.join(".git");
                let p = |x: &std::path::Path| x.display().to_string();
                vec![
                    kv("GIT_DIR", p(&git)),
                    kv("GIT_WORK_TREE", p(&decoy)),
                    kv("GIT_COMMON_DIR", p(&git)),
                    kv("GIT_INDEX_FILE", p(&git.join("index"))),
                    kv("GIT_OBJECT_DIRECTORY", p(&git.join("objects"))),
                    kv("GIT_ALTERNATE_OBJECT_DIRECTORIES", p(&git.join("objects"))),
                    kv("GIT_NAMESPACE", "xver-1181".to_string()),
                    kv("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1".to_string()),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the sandbox project needs a commit (a dispatch branches a
    /// worktree from `HEAD`).
    pub fn needs_commit(self) -> bool {
        matches!(
            self,
            Probe::PasteEnvelope | Probe::SignalAck | Probe::GitEnv
        )
    }

    /// Whether the probe runs the synthetic Claude Code stand-in.
    pub fn needs_stub(self) -> bool {
        matches!(self, Probe::LateSessionStart | Probe::PasteEnvelope)
    }

    /// The global `default_command` a dispatch probe's unit runs — `dispatch
    /// --single` reads it from `DOT_AGENT_DECK_CONFIG`, never from the project
    /// file — or `None` when the probe dispatches nothing.
    ///
    /// `PasteEnvelope` needs a Claude-typed unit (the stub, reporting the
    /// envelope); the other two need only a unit that records what it was
    /// handed, so a `tee` into the run's artifacts, whose command types as no
    /// agent at all.
    pub fn dispatch_command(self, sb: &Sandbox) -> Option<String> {
        match self {
            Probe::PasteEnvelope => Some(format!(
                "{} --xver-mode=paste-envelope",
                sb.stub_claude().display()
            )),
            Probe::SignalAck | Probe::GitEnv => Some(format!(
                "printf 'XVER_DISPATCHED_SENTINEL\\n'; exec tee -a {}",
                dispatched_record(sb).display()
            )),
            _ => None,
        }
    }
}

pub const ROLE_ALPHA: &str = "alpha";
pub const ROLE_BETA: &str = "beta";
pub const ROLE_LATEBOOT: &str = "lateboot";

/// #1190's decoy repository — a standalone repository under `$S`, never a
/// worktree of anything.
pub fn decoy_repo(sb: &Sandbox) -> std::path::PathBuf {
    sb.root.join("decoy")
}

/// Where a `tee` dispatch unit records what the daemon handed it.
pub fn dispatched_record(sb: &Sandbox) -> std::path::PathBuf {
    sb.artifacts.join("dispatched-unit.txt")
}

/// One role a probe adds to the fixture's orchestration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraRole {
    pub name: String,
    pub command: String,
    pub clear: bool,
}

/// The fixture for a run: [`crate::sandbox::FIXTURE_TOML`] with the probe's
/// roles appended, in order, to the same orchestration.
pub fn fixture(sb: &Sandbox, probe: Probe) -> String {
    let mut s = crate::sandbox::FIXTURE_TOML.to_string();
    for role in probe.extra_roles(sb) {
        s.push_str(&format!(
            "\n[[orchestrations.roles]]\nname = {}\ncommand = {}\n",
            toml_string(&role.name),
            toml_string(&role.command)
        ));
        if role.clear {
            s.push_str("clear = true\n");
        }
    }
    s
}

/// A TOML basic string.
pub fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn each_swept_branch_selects_its_own_probe() {
        for (branch, want) in [
            ("agent/dispatch-issue-1109", Probe::TeardownInventory),
            ("agent/dispatch-issue-1031", Probe::LateSessionStart),
            ("agent/dispatch-issue-1082", Probe::LogEscaping),
            ("agent/dispatch-issue-1121", Probe::DiscoveryFallback),
            ("agent/dispatch-issue-1182", Probe::PasteEnvelope),
            ("agent/dispatch-issue-925", Probe::CrossPaneSessionKey),
            ("agent/dispatch-issue-1129", Probe::SignalAck),
            ("agent/dispatch-issue-1181", Probe::GitEnv),
            ("agent/dispatch-issue-1", Probe::Generic),
            ("main", Probe::Generic),
        ] {
            assert_eq!(Probe::for_branch(branch), want, "{branch}");
        }
    }

    #[test]
    fn the_discovery_probe_refuses_any_arm_but_the_no_xdg_fallback() {
        assert!(
            Probe::DiscoveryFallback
                .check_config(EndpointMode::Resolved, false)
                .is_ok()
        );
        for (mode, keep) in [
            (EndpointMode::Resolved, true),
            (EndpointMode::SandboxSockets, false),
        ] {
            assert!(
                Probe::DiscoveryFallback.check_config(mode, keep).is_err(),
                "{mode:?} keep_xdg={keep}"
            );
        }
        assert!(
            Probe::LogEscaping
                .check_config(EndpointMode::Resolved, false)
                .is_err()
        );
        assert!(
            Probe::LogEscaping
                .check_config(EndpointMode::SandboxSockets, true)
                .is_ok()
        );
    }

    #[test]
    fn a_probe_fixture_is_the_base_fixture_with_its_roles_appended() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        assert_eq!(
            fixture(&sb, Probe::Generic),
            crate::sandbox::FIXTURE_TOML,
            "no probe, no change"
        );
        let f = fixture(&sb, Probe::LateSessionStart);
        assert!(f.starts_with(crate::sandbox::FIXTURE_TOML));
        assert!(f.contains("name = \"lateboot\""), "{f}");
        assert!(
            f.contains("command = \"/srv/runs/r1/stub/claude --xver-mode=late-boot\""),
            "{f}"
        );
        assert!(f.trim_end().ends_with("clear = true"), "{f}");
        let f = fixture(&sb, Probe::CrossPaneSessionKey);
        assert!(f.contains("name = \"alpha\"") && f.contains("name = \"beta\""));
        assert!(!f.contains("clear = true"));
    }

    #[test]
    fn toml_strings_escape_what_toml_requires() {
        assert_eq!(toml_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
        assert_eq!(toml_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn the_git_probe_points_all_eight_location_variables_at_the_decoy() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let env = Probe::GitEnv.extra_env(&sb);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for want in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        assert!(
            !names.contains(&"GIT_CEILING_DIRECTORIES"),
            "#1181 deliberately leaves it alone"
        );
        for (k, v) in &env {
            if k.starts_with("GIT_") && v.starts_with('/') {
                assert!(v.starts_with("/srv/runs/r1/decoy"), "{k}={v}");
            }
        }
        assert!(Probe::Generic.extra_env(&sb).is_empty());
    }

    #[test]
    fn only_dispatch_probes_get_a_unit_command_and_a_commit() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        for p in [Probe::PasteEnvelope, Probe::SignalAck, Probe::GitEnv] {
            assert!(
                p.dispatch_command(&sb).is_some() && p.needs_commit(),
                "{p:?}"
            );
        }
        assert!(
            Probe::PasteEnvelope
                .dispatch_command(&sb)
                .is_some_and(|c| c.starts_with("/srv/runs/r1/stub/claude ")),
            "the paste probe's unit must be typed as Claude Code"
        );
        for p in [
            Probe::Generic,
            Probe::TeardownInventory,
            Probe::LogEscaping,
            Probe::DiscoveryFallback,
        ] {
            assert!(
                p.dispatch_command(&sb).is_none() && !p.needs_commit(),
                "{p:?}"
            );
        }
    }
}
