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
//! A probe is named with `--probe`, or selected by `--probe auto` from the
//! branch's `dispatch-issue-<n>` path component ([`Probe::for_branch`]), and
//! ONLY runs in the reverse direction: that is the pairing that executes a
//! daemon-side change. Auto never falls back: a branch it cannot tie to one
//! probe — no such component, more than one, or an issue no probe was written
//! for — is refused, and `--probe generic` (the four tells plus the `role-set`
//! tell, and no stimulus) is how to ask for that run on purpose. The evidence
//! file names the probe and how it was chosen, so it never implies more was
//! measured.

use std::collections::BTreeSet;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::sandbox::{EndpointMode, Sandbox};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum Probe {
    /// No stimulus: the four tells, plus the `role-set` tell (see
    /// [`Probe::asserts_role_set`]).
    #[default]
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
    /// A COMPATIBILITY check: an old client never reads the acknowledgement, so
    /// a daemon that writes none passes it too.
    SignalAck,
    /// PR #1190 / issue #1181: an old-CLI dispatch under hostile ambient git
    /// location variables changes only the intended repository.
    GitEnv,
}

/// The path component a dispatched branch carries its issue number in, as
/// `dispatch-issue-<n>` (`agent/dispatch-issue-1181`).
const ISSUE_COMPONENT: &str = "dispatch-issue-";

/// The issue number in one path component, when the WHOLE component is
/// `dispatch-issue-<digits>` — so `dispatch-issue-1181-v2` and
/// `dispatch-issue-1109-1121` name none.
fn component_issue(component: &str) -> Option<u32> {
    let digits = component.strip_prefix(ISSUE_COMPONENT)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

impl Probe {
    /// The probe written for `issue` ([`Probe::origin`]), if any.
    pub fn for_issue(issue: u32) -> Option<Probe> {
        Probe::value_variants()
            .iter()
            .copied()
            .find(|p| p.origin().is_some_and(|(_, i)| i == issue))
    }

    /// The probe `--probe auto` selects for a reverse run of `branch`, with
    /// the issue it was selected by — or why auto cannot select one, for the
    /// caller to refuse the run with.
    ///
    /// The issue is read from exactly one path component that is exactly
    /// `dispatch-issue-<n>`, never from wherever digits happen to appear:
    /// taking the last run of digits selected `generic` for
    /// `agent/dispatch-issue-1181-v2` and #1121's probe for
    /// `agent/dispatch-issue-1109-1121` (Greptile, PR #1210). Anything short
    /// of one component naming an issue a probe was written for is refused
    /// rather than run as `generic`, because in reverse the branch-specific
    /// probe is the point: a silent downgrade would produce an evidence file
    /// that measured nothing the branch changed. Every refusal names each
    /// issue with a probe whose number the branch name contains anywhere, as a
    /// hint, never as a selection.
    pub fn for_branch(branch: &str) -> Result<(Probe, u32), String> {
        const ASK: &str = "Pass `--probe <name>` for the probe written for this branch's change, \
                           or `--probe generic` for the four tells plus `role-set` and no \
                           branch-specific stimulus.";
        let issues: BTreeSet<u32> = branch.split('/').filter_map(component_issue).collect();
        let issue = match issues.iter().copied().collect::<Vec<_>>()[..] {
            [issue] => issue,
            [] => {
                return Err(format!(
                    "`--probe auto` cannot identify the probe for `{branch}`: no path component \
                     of it is exactly `{ISSUE_COMPONENT}<n>`, the form a dispatched branch carries \
                     its issue number in.{} {ASK}",
                    mentioned(branch)
                ));
            }
            ref several => {
                return Err(format!(
                    "`--probe auto` cannot identify the probe for `{branch}`: it has more than \
                     one `{ISSUE_COMPONENT}<n>` component (issues {}).{} {ASK}",
                    several
                        .iter()
                        .map(|i| format!("#{i}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    mentioned(branch)
                ));
            }
        };
        Probe::for_issue(issue).map(|p| (p, issue)).ok_or_else(|| {
            format!(
                "`--probe auto` found issue #{issue} in `{branch}`, and no probe was written for \
                 it.{} {ASK}",
                mentioned(branch)
            )
        })
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

    /// One line for the evidence file, which renders it for reverse runs only.
    pub fn describe(self) -> String {
        match (self, self.origin()) {
            (Probe::Generic, _) | (_, None) => {
                "generic — the four tells plus `role-set`; no branch-specific stimulus".to_string()
            }
            (Probe::SignalAck, Some((pr, issue))) => format!(
                "`{}` — PR #{pr} / issue #{issue}; a COMPATIBILITY check, which a daemon writing \
                 no acknowledgement passes too",
                Probe::SignalAck.name()
            ),
            (p, Some((pr, issue))) => format!("`{}` — PR #{pr} / issue #{issue}", p.name()),
        }
    }

    /// The id of the tell this probe's own stimulus records (`probes.rs`), or
    /// `None` for a probe with no stimulus: `Generic`, and `DiscoveryFallback`,
    /// whose measurement is the attach itself.
    ///
    /// A reverse run that found the daemon is not complete without it
    /// (`report::Evidence::expected_tells`), so a probe whose stimulus never
    /// ran cannot aggregate to a PASS on the four tells alone.
    pub fn tell_id(self) -> Option<&'static str> {
        match self {
            Probe::Generic | Probe::DiscoveryFallback => None,
            Probe::TeardownInventory => Some("probe-teardown-inventory"),
            Probe::LateSessionStart => Some("probe-late-session-start"),
            Probe::LogEscaping => Some("probe-log-escaping"),
            Probe::PasteEnvelope => Some("probe-paste-envelope"),
            Probe::CrossPaneSessionKey => Some("probe-cross-pane-session-key"),
            Probe::SignalAck => Some("probe-signal-ack"),
            Probe::GitEnv => Some("probe-git-env"),
        }
    }

    /// Whether a reverse run asserts the `role-set` tell (see
    /// `inner::judge_role_set`): the attached old TUI left exactly one set of
    /// the orchestration's roles, the one the setup TUI brought up, under the
    /// one listening daemon.
    ///
    /// `Generic` only. It is what separates the #1179 control's expected result
    /// from #1179's failure on the path where the old TUI DOES find the daemon:
    /// tell 1 catches a second daemon but not a role set that the old TUI's
    /// session restore spawned into the existing one. The probes keep the tells
    /// their evidence was recorded against.
    pub fn asserts_role_set(self) -> bool {
        self == Probe::Generic
    }

    /// Refuse a probe whose run configuration cannot reach its changed arm.
    ///
    /// `DiscoveryFallback` is the one with a hard requirement: #1121 moved the
    /// endpoint only in the no-XDG, no-override fallback, so any other
    /// configuration measures an arm it did not touch.
    ///
    /// `Generic` has no changed arm to reach, so it accepts every endpoint
    /// mode. That is what makes the #1179 negative control runnable: the same
    /// reverse `resolved` / no-XDG run as `discovery-fallback`, against a
    /// branch whose daemon binds the flat pair, where `discovery-fallback`
    /// itself refuses (`probes::daemon_layout_precondition`). In that
    /// configuration the inner half learns which pair the daemon bound, holds
    /// every other candidate absent, and classifies a missing prompt exactly as
    /// it does for `discovery-fallback` (`inner::classify_undiscovered`).
    pub fn check_config(self, mode: EndpointMode, keep_xdg: bool) -> Result<(), String> {
        match self {
            Probe::DiscoveryFallback if mode != EndpointMode::Resolved || keep_xdg => Err(
                "the discovery-fallback probe needs `--endpoint-mode resolved \
                 --unset-xdg-runtime-dir`: #1121 changed only the no-XDG, no-override fallback \
                 arm, and any other configuration resolves an arm it did not touch"
                    .to_string(),
            ),
            Probe::DiscoveryFallback | Probe::Generic => Ok(()),
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
    ///
    /// **These are trusted, hardcoded values, and their confinement is this
    /// function's job.** `sandbox::check_env` admits each extra by exact name
    /// and exact value, refuses a base-allowlist or credential-shaped name, and
    /// nothing more: it does NOT check that a path-valued extra stays under
    /// `$S`. So a path-valued extra must be built from the sandbox here, as the
    /// `GitEnv` values are from [`decoy_repo`]. If extras ever become
    /// data-driven or caller-selectable, add a per-variable validator —
    /// canonical containment under `$S` for a path or a path list — before
    /// admitting them; the exact-value check alone would admit a host path.
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

/// A refusal's hint: each issue with a probe whose number `branch` contains as
/// a whole run of digits, anywhere. Empty when there is none.
fn mentioned(branch: &str) -> String {
    let named: BTreeSet<u32> = branch
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|run| run.parse().ok())
        .collect();
    let hints: Vec<String> = named
        .into_iter()
        .filter_map(|i| Probe::for_issue(i).map(|p| format!("#{i} (`{}`)", p.name())))
        .collect();
    if hints.is_empty() {
        String::new()
    } else {
        format!(
            " Its name mentions {}: if one is this branch's issue, pass its probe.",
            hints.join(" and ")
        )
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
        for (branch, want, issue) in [
            ("agent/dispatch-issue-1109", Probe::TeardownInventory, 1109),
            ("agent/dispatch-issue-1031", Probe::LateSessionStart, 1031),
            ("agent/dispatch-issue-1082", Probe::LogEscaping, 1082),
            ("agent/dispatch-issue-1121", Probe::DiscoveryFallback, 1121),
            ("agent/dispatch-issue-1182", Probe::PasteEnvelope, 1182),
            ("agent/dispatch-issue-925", Probe::CrossPaneSessionKey, 925),
            ("agent/dispatch-issue-1129", Probe::SignalAck, 1129),
            ("agent/dispatch-issue-1181", Probe::GitEnv, 1181),
        ] {
            assert_eq!(Probe::for_branch(branch), Ok((want, issue)), "{branch}");
        }
    }

    #[test]
    fn every_probe_with_a_stimulus_or_an_attach_is_reachable_by_its_issue() {
        for &probe in Probe::value_variants() {
            match probe.origin() {
                None => assert_eq!(probe, Probe::Generic),
                Some((_, issue)) => {
                    assert_eq!(Probe::for_issue(issue), Some(probe), "{}", probe.name());
                    assert_eq!(
                        Probe::for_branch(&format!("agent/dispatch-issue-{issue}")),
                        Ok((probe, issue))
                    );
                }
            }
        }
    }

    /// Greptile's finding on PR #1210: the last run of digits selected
    /// `generic` here, silently.
    #[test]
    fn a_suffixed_branch_is_refused_not_downgraded_to_generic() {
        let err =
            Probe::for_branch("agent/dispatch-issue-1181-v2").expect_err("no exact component");
        assert!(err.contains("exactly `dispatch-issue-<n>`"), "{err}");
        assert!(
            err.contains("#1181 (`git-env`)"),
            "names the likely probe: {err}"
        );
        assert!(err.contains("--probe generic"), "{err}");
    }

    /// Greptile's finding on PR #1210: the last run of digits selected #1121's
    /// probe here, for what is at least as likely a #1109 branch.
    #[test]
    fn a_branch_naming_two_issues_is_refused_not_resolved_to_the_last() {
        let err = Probe::for_branch("agent/dispatch-issue-1109-1121").expect_err("ambiguous");
        assert!(
            err.contains("#1109 (`teardown-inventory`) and #1121 (`discovery-fallback`)"),
            "{err}"
        );
        let err = Probe::for_branch("dispatch-issue-1109/dispatch-issue-1121")
            .expect_err("two components");
        assert!(
            err.contains("more than one") && err.contains("#1109, #1121"),
            "{err}"
        );
        assert!(err.contains("#1109 (`teardown-inventory`)"), "{err}");
    }

    #[test]
    fn the_component_is_matched_whole_and_anywhere_in_the_path() {
        assert_eq!(
            Probe::for_branch("feature/dispatch-issue-1181/retry"),
            Ok((Probe::GitEnv, 1181)),
            "one exact component, whatever surrounds it"
        );
        for branch in [
            "agent/xdispatch-issue-1181",
            "agent/dispatch-issue-",
            "agent/dispatch-issue-11a81",
            "agent/issue-1181",
        ] {
            assert!(Probe::for_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn a_branch_with_no_issue_or_an_issue_without_a_probe_is_refused() {
        let err = Probe::for_branch("main").expect_err("no issue");
        assert!(err.contains("no path component"), "{err}");
        assert!(!err.contains("mentions"), "no hint to give: {err}");
        let err = Probe::for_branch("agent/dispatch-issue-1").expect_err("no probe for #1");
        assert!(
            err.contains("found issue #1") && err.contains("no probe"),
            "{err}"
        );
        assert!(
            !err.contains("mentions"),
            "no issue with a probe to hint at: {err}"
        );
        let err =
            Probe::for_branch("agent/dispatch-issue-1/after-1181").expect_err("#1 has no probe");
        assert!(
            err.contains("found issue #1") && err.contains("#1181 (`git-env`)"),
            "{err}"
        );
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
    fn the_generic_probe_runs_in_the_1179_control_configuration_and_every_other() {
        // `--probe generic --endpoint-mode resolved --unset-xdg-runtime-dir`,
        // the #1179 negative control, used to be refused here.
        assert!(
            Probe::Generic
                .check_config(EndpointMode::Resolved, false)
                .is_ok()
        );
        for (mode, keep) in [
            (EndpointMode::Resolved, true),
            (EndpointMode::SandboxSockets, true),
            (EndpointMode::SandboxSockets, false),
        ] {
            assert!(
                Probe::Generic.check_config(mode, keep).is_ok(),
                "{mode:?} keep_xdg={keep}"
            );
        }
        // Only `generic` was widened: every stimulus probe still needs the
        // sandbox sockets it was written against.
        for p in [
            Probe::TeardownInventory,
            Probe::LateSessionStart,
            Probe::LogEscaping,
            Probe::PasteEnvelope,
            Probe::CrossPaneSessionKey,
            Probe::SignalAck,
            Probe::GitEnv,
        ] {
            assert!(
                p.check_config(EndpointMode::Resolved, false).is_err(),
                "{p:?}"
            );
        }
    }

    #[test]
    fn only_the_generic_probe_asserts_the_role_set() {
        assert!(Probe::Generic.asserts_role_set());
        for p in [
            Probe::TeardownInventory,
            Probe::LateSessionStart,
            Probe::LogEscaping,
            Probe::DiscoveryFallback,
            Probe::PasteEnvelope,
            Probe::CrossPaneSessionKey,
            Probe::SignalAck,
            Probe::GitEnv,
        ] {
            assert!(!p.asserts_role_set(), "{p:?}");
        }
        assert!(Probe::Generic.describe().contains("role-set"));
    }

    #[test]
    fn signal_ack_is_described_as_a_compatibility_check() {
        let d = Probe::SignalAck.describe();
        assert!(d.contains("COMPATIBILITY"), "{d}");
        assert!(
            d.starts_with("`signal-ack` — PR #1188 / issue #1129"),
            "{d}"
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
