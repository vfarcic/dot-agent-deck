use std::path::Path;

use serde::Deserialize;

use crate::event::AgentType;

pub const CONFIG_FILE_NAME: &str = ".dot-agent-deck.toml";

#[derive(Debug)]
pub enum ProjectConfigError {
    Io {
        path: String,
        source: std::io::Error,
    },
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

/// Issue #308 follow-up: the ONE place a `ProjectConfigError` becomes terminal
/// output, and therefore the one place it is made safe to be terminal output.
///
/// Hand-written rather than `thiserror`-derived precisely so this escaping
/// exists — the derive would emit the two format strings straight through.
///
/// Both variants interpolate untrusted text. `path` is whatever directory the
/// caller passed (`dot-agent-deck validate <path>`), and `source` on the
/// `Parse` variant is a `toml` error that renders **the offending source line
/// verbatim**, so real `0x1B` bytes in a `.dot-agent-deck.toml` reach the
/// terminal:
///
/// ```text
/// Failed to parse …: TOML parse error at line 3, column 10
///   |
/// 3 | bogus = <ESC>[31mPWNED<ESC>[0m
/// ```
///
/// That is the same class and the same delivery vector as
/// [`crate::config_validation::ValidationIssue`]'s seam, which this PR sealed —
/// but strictly wider, because it needs no *valid* config at all: a file that
/// merely fails to parse is enough, and `.dot-agent-deck.toml` travels with a
/// repository (a clone, a contributor branch, a PR checkout).
///
/// Sealed here at `Display` rather than at `main.rs`'s `eprintln!("{e}")` for
/// the reason the `ValidationIssue` seam records: one seam covers every sink by
/// construction and cannot be forgotten by a later addition. That is not
/// hypothetical for this type — `validate` is only its most obvious sink;
/// `dispatch::list_targets` folds the same error into a message rendered to a
/// coding agent's terminal, and the hydration path logs it with `error = %e`.
///
/// [`escape_multiline_for_terminal`](crate::config_validation) keeps the
/// frame's own newlines, so a genuine syntax error stays exactly as readable as
/// it is today — the gutter rendering is the whole value of a `toml` error, and
/// escaping it to a single `\n`-riddled line would trade one defect for
/// another.
impl std::fmt::Display for ProjectConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let raw = match self {
            Self::Io { path, source } => format!("Failed to read {path}: {source}"),
            Self::Parse { path, source } => format!("Failed to parse {path}: {source}"),
        };
        f.write_str(&crate::config_validation::escape_multiline_for_terminal(
            &raw,
        ))
    }
}

/// Kept from the `thiserror` derive this type used to carry, so the error chain
/// stays intact for programmatic inspection.
///
/// Note that a caller who walked the chain and printed a link directly would be
/// printing the `toml` crate's own error — the raw value the `Display` above
/// exists to wrap — so escaping is a property of rendering *this* type, not of
/// everything reachable from it. Nothing in the deck walks it; every sink
/// renders the error itself with `{e}` or `%e`.
impl std::error::Error for ProjectConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub modes: Vec<ModeConfig>,
    #[serde(default)]
    pub orchestrations: Vec<OrchestrationConfig>,
    /// PRD #126: how long the daemon waits for a delegated worker to send
    /// `work-done` before injecting an idle prompt into the orchestrator's
    /// pane (see [`crate::state::worker_response_timeout`]). Absent from a
    /// config means the [`DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES`] default,
    /// so existing configs keep working untouched.
    ///
    /// **Accepted values** (PRD #126 M1 audit finding 4 — must flow into the
    /// M3.1 docs page):
    ///
    /// * `0` — **detector disabled**. No idle watch is armed for this
    ///   orchestration's delegations, so a silent worker is never reported and
    ///   no timer is created. `0` does NOT mean "report immediately": that
    ///   raced the worker's own dispatch and reported every worker as stuck
    ///   before it had a chance to answer.
    /// * `1` ..=
    ///   [`MAX_WORKER_RESPONSE_TIMEOUT_MINUTES`](crate::state::MAX_WORKER_RESPONSE_TIMEOUT_MINUTES)
    ///   (7 days) — honored as written.
    /// * anything larger — rejected with a warning; the daemon falls back to
    ///   [`DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES`]. Such a value is
    ///   indistinguishable from "disabled" while still costing a live watch
    ///   task, so it is treated as a misconfiguration rather than honored.
    ///
    /// ⚠️ TOML placement: this is a **top-level scalar**, so it must appear
    /// *before* the first table header (`[[modes]]` / `[[orchestrations]]`).
    /// Appended at the end of the file it would silently become a key of the
    /// last table and be ignored.
    #[serde(default = "default_worker_response_timeout_minutes")]
    pub worker_response_timeout_minutes: u64,
}

/// PRD #126: two hours — long enough that a worker chewing through a real
/// task is never nagged, short enough that a genuinely stuck delegation
/// surfaces within one working session.
pub const DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES: u64 = 120;

fn default_worker_response_timeout_minutes() -> u64 {
    DEFAULT_WORKER_RESPONSE_TIMEOUT_MINUTES
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeConfig {
    pub name: String,
    /// Issue #308: what agent this mode's **agent pane** runs, when the command
    /// entered for it cannot reveal that by itself.
    ///
    /// The agent pane's command is typed in the new-pane form rather than
    /// written here, so this key does not name a command — it answers the one
    /// question the command may be unable to: `devbox run codex-big`,
    /// `mise exec -- codex` or a bespoke `run-codex.sh` all resolve to a
    /// *launcher* basename, and [`crate::event::AgentType::from_command`]
    /// correctly refuses to guess what is behind it.
    ///
    /// Resolved by [`Self::declared_agent_type`] through
    /// [`crate::agent_registry::resolve_declared_agent`] — the same rule
    /// `wrap --agent` applies, so an unrecognized name yields the neutral
    /// [`crate::event::AgentType::None`] rather than a guess. Absent (the
    /// default, and every config written before this key existed) means "infer
    /// from the command", i.e. exactly the previous behavior.
    ///
    /// Deliberately on `[[modes]]` and NOT on `[[modes.panes]]`: the persistent
    /// side panes run tools, not agents, and never pass through the seam this
    /// declaration steers.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub init_command: Option<String>,
    /// PRD #127 M3.1: a prompt auto-delivered to the mode's **agent** pane
    /// once the agent signals readiness (gated like orchestrations), as
    /// opposed to `init_command` which targets the side panes. Optional;
    /// `None` (the default, and existing configs without it) delivers nothing.
    /// This is the generic primitive the Phase-3 "schedule" creation mode
    /// builds on — a `[[modes]]` entry that carries a `seed_prompt`.
    #[serde(default)]
    pub seed_prompt: Option<String>,
    #[serde(default)]
    pub panes: Vec<ModePersistentPane>,
    #[serde(default)]
    pub rules: Vec<ModeRule>,
    #[serde(default = "default_reactive_panes")]
    pub reactive_panes: usize,
}

fn default_reactive_panes() -> usize {
    2
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModePersistentPane {
    pub command: String,
    pub name: Option<String>,
    #[serde(default = "default_pane_watch")]
    pub watch: bool,
}

fn default_pane_watch() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeRule {
    pub pattern: String,
    #[serde(default)]
    pub watch: bool,
    pub interval: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrchestrationConfig {
    #[serde(default)]
    pub name: String,
    pub roles: Vec<OrchestrationRoleConfig>,
}

/// PRD #111: minimal description of one occupied role slot, as seen by
/// the TUI hydration path when rebuilding orchestration tabs after a
/// reconnect. `role_index` is the slot's position in the daemon's
/// `OrchestrationConfig.roles`; `role_name` and `is_start_role` come
/// from the daemon's `TabMembership::Orchestration` payload. Defined
/// here (rather than in `ui.rs`) so the synthesise helper can stay
/// next to `OrchestrationConfig` without a back-edge import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesisRoleSlot {
    pub role_index: usize,
    pub role_name: String,
    pub is_start_role: bool,
}

impl OrchestrationConfig {
    /// PRD #111: synthesise a minimal `OrchestrationConfig` from
    /// daemon-supplied bucket metadata when the local
    /// `.dot-agent-deck.toml` cannot be loaded (laptop TUI reconnecting
    /// to a VM daemon whose `bucket.cwd` doesn't resolve locally).
    ///
    /// The resulting config is structurally correct — `name` matches
    /// the daemon's resolved orchestration name and `roles.len()` is
    /// `max(role_index) + 1` so
    /// `open_orchestration_tab_with_existing_role_panes`'s length check
    /// passes — but the display-only fields (`command`, `description`,
    /// `prompt_template`) are left as defaults. Tab rendering, status
    /// tracking, and daemon-side delegation still work; only the
    /// pre-rendered orchestrator-context.md enrichment is missing.
    ///
    /// Roles whose `role_index` had no surviving pane keep a synthetic
    /// `name = "role-{i}"` placeholder so the rendered sidebar doesn't
    /// show an empty label.
    pub fn synthesize_from_bucket_metadata(name: &str, slots: &[SynthesisRoleSlot]) -> Self {
        let max_index = slots.iter().map(|s| s.role_index).max().unwrap_or(0);
        // PRD #120 H1 (defense in depth): the wire-boundary validators
        // (`validate_tab_membership` for reconnect, `validate_orchestration_surface`
        // for the live-surface path) cap `role_index` at
        // `ORCHESTRATION_ROLE_INDEX_MAX`, but guard here too so a direct/internal
        // caller can't OOM or hit the debug-mode overflow panic. `saturating_add`
        // avoids the `usize::MAX + 1` panic; the `.min(..)` bounds the placeholder
        // allocation even if an over-cap index slips through. A slot whose
        // `role_index` lands past the clamped count is then skipped by the bounds-
        // checked `roles.get_mut(..)` below (and `claimed[..]` is only indexed via
        // the same short-circuit), so the clamp is safe.
        let role_count = if slots.is_empty() {
            0
        } else {
            max_index
                .saturating_add(1)
                .min(crate::agent_pty::ORCHESTRATION_ROLE_INDEX_MAX + 1)
        };
        let mut roles: Vec<OrchestrationRoleConfig> = (0..role_count)
            .map(|i| OrchestrationRoleConfig {
                agent: None,
                name: format!("role-{i}"),
                command: String::new(),
                start: false,
                description: None,
                prompt_template: None,
                clear: true,
            })
            .collect();
        // PRD #111 reviewer S2: first-wins on duplicate `role_index`,
        // matching the hydration loop's duplicate-pane handling at
        // `src/ui.rs::hydration` (`role_pane_ids[role_index].is_some()` →
        // keep the first slot, drop the rest). Without this guard the
        // two paths drifted: hydration kept the first pane_id while
        // synthesis kept the *last* role_name, producing a tab whose
        // role label and live pane came from different bucket entries.
        // The daemon is not supposed to emit duplicates, but if it does
        // the two paths must at least agree on which slot wins.
        let mut claimed = vec![false; role_count];
        for slot in slots {
            if let Some(role) = roles.get_mut(slot.role_index)
                && !claimed[slot.role_index]
            {
                claimed[slot.role_index] = true;
                if !slot.role_name.is_empty() {
                    role.name = slot.role_name.clone();
                }
                if slot.is_start_role {
                    role.start = true;
                }
            }
        }
        OrchestrationConfig {
            name: name.to_string(),
            roles,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrchestrationRoleConfig {
    pub name: String,
    pub command: String,
    /// Issue #308: what agent `command` actually launches, when the command
    /// itself cannot reveal it.
    ///
    /// [`crate::event::AgentType::from_command`] resolves an agent by the
    /// command's basename, so `devbox run -- codex`, `mise exec -- codex`,
    /// `make codex` and a bespoke `run-codex.sh` all resolve to nothing —
    /// correctly, since no parser can see through an arbitrary launcher. The
    /// consequences are visible: the role's card reads "No agent", and because
    /// failed detection also drops the wrapper, a Codex role stays unidentified
    /// until its first delegated task (Codex posts its native `SessionStart`
    /// only when a turn begins, so the wrapper's fork-time one is the only
    /// event that could badge the pane earlier).
    ///
    /// Resolved by [`Self::declared_agent_type`] through
    /// [`crate::agent_registry::resolve_declared_agent`] — the same rule
    /// `wrap --agent` applies, so an unrecognized name yields the neutral
    /// [`crate::event::AgentType::None`] rather than a guess. Absent (the
    /// default, and every config written before this key existed) means "infer
    /// from `command`", i.e. exactly the previous behavior.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub start: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub prompt_template: Option<String>,
    #[serde(default = "default_clear")]
    pub clear: bool,
}

fn default_clear() -> bool {
    true
}

/// Issue #308: resolve a config-declared agent name to an [`AgentType`].
///
/// The shared body behind [`OrchestrationRoleConfig::declared_agent_type`] and
/// [`ModeConfig::declared_agent_type`], so both surfaces answer a given name
/// identically — and, through
/// [`crate::agent_registry::resolve_declared_agent`], identically to
/// `wrap --agent <name>`.
///
/// The three-way distinction in the return type is the whole contract:
///
/// * `None` — **no declaration**. The key is absent, or holds only whitespace.
///   The caller falls back to deriving the type from the command, which is
///   what every config written before this key existed does.
/// * `Some(AgentType::None)` — **declared, but no shipped agent claims that
///   name**. Still a declaration: the user answered the question, so the answer
///   stands and the command is not consulted. The pane gets no agent and no
///   wrapper, which is the same thing `wrap --agent <typo>` produces, and
///   `dot-agent-deck validate` warns about the name.
/// * `Some(real)` — **declared and recognized**. This wins over the command.
///
/// An empty or whitespace-only value maps to "no declaration" rather than to
/// `AgentType::None`: `agent = ""` reads as *unset*, and treating it as an
/// explicit "no agent" would silently strip the wrapper off an otherwise
/// perfectly inferable `codex …` command. Trimming here (rather than inside
/// [`crate::agent_registry::resolve_declared_agent`]) keeps the shared resolver
/// byte-exact with the argv slot `--agent` reads, while still being forgiving
/// about a TOML value a human typed with a stray space.
fn declared_agent_type(declared: Option<&str>) -> Option<AgentType> {
    let name = declared?.trim();
    if name.is_empty() {
        return None;
    }
    Some(crate::agent_registry::resolve_declared_agent(name))
}

impl OrchestrationRoleConfig {
    /// This role's DECLARED agent type — see [`declared_agent_type`] for what
    /// each of the three answers means. `None` when the role declares nothing.
    pub fn declared_agent_type(&self) -> Option<AgentType> {
        declared_agent_type(self.agent.as_deref())
    }

    /// What agent this role runs: the declaration if it made one, otherwise the
    /// type derived from `command`.
    ///
    /// This is the single answer every spawn seam for an orchestration role
    /// asks — the TUI's `Ctrl+N` path, the daemon's dispatch primitive, and the
    /// `clear = true` re-create — so a declared role launches identically
    /// whichever of them created it.
    ///
    /// Declared outranks derived because the two are read in the SAME pass over
    /// the SAME file: the declaration cannot be stale against the command it
    /// sits beside. That is exactly what distinguishes it from
    /// [`crate::agent_pty::RunningAgent::spawn_agent_type`], the frozen
    /// spawn-time identity, which PRD #225 finding 1 deliberately demoted to a
    /// fallback *because* it was captured at a previous spawn and can disagree
    /// with an edited command.
    pub fn resolved_agent_type(&self) -> Option<AgentType> {
        self.declared_agent_type()
            .or_else(|| AgentType::from_command(Some(&self.command)))
    }
}

impl ModeConfig {
    /// This mode's DECLARED agent-pane type — see [`declared_agent_type`] for
    /// what each of the three answers means. `None` when the mode declares
    /// nothing.
    pub fn declared_agent_type(&self) -> Option<AgentType> {
        declared_agent_type(self.agent.as_deref())
    }

    /// What agent this mode's agent pane runs, given the `command` the user
    /// entered for it: the declaration if the mode made one, otherwise the type
    /// derived from that command.
    ///
    /// A mode's agent command is typed in the new-pane form rather than stored
    /// in the config, so unlike [`OrchestrationRoleConfig::resolved_agent_type`]
    /// this takes the command as an argument.
    pub fn resolved_agent_type(&self, command: &str) -> Option<AgentType> {
        self.declared_agent_type()
            .or_else(|| AgentType::from_command(Some(command)))
    }
}

/// Resolve an orchestration name with the cwd-basename fallback that
/// the TUI applies when constructing `TabMembership::Orchestration` and
/// when labelling the `Tab::Orchestration` record. Empty / whitespace
/// config names — produced by `#[serde(default)]` on `OrchestrationConfig::name`
/// or by the user not writing a `name = ...` line — resolve to the
/// basename of `dir`; falls back to the path's `display()` form when the
/// dir has no basename (e.g. `/`).
///
/// Centralized so the TUI's tab construction site, the TUI's hydration
/// site, and the daemon's `handle_delegate` lookup all agree on the
/// resolved-name string. Without this single-source contract, an
/// unnamed orchestration's TabMembership carries the basename but the
/// daemon's freshly-loaded config still has `name = ""`, and
/// `handle_delegate`'s `orch.name == orchestration_name` lookup
/// misses — silently dropping per-role `prompt_template` wrapping
/// (round-10 reviewer #1).
pub fn resolve_orchestration_name(config_name: &str, dir: &Path) -> String {
    if !config_name.is_empty() {
        return config_name.to_string();
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string())
}

pub fn load_project_config(dir: &Path) -> Result<Option<ProjectConfig>, ProjectConfigError> {
    let path = dir.join(CONFIG_FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let mut config: ProjectConfig =
                toml::from_str(&contents).map_err(|source| ProjectConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })?;
            // Round-10 reviewer #1: normalize empty orchestration names
            // to the cwd-basename fallback at load time, so the daemon's
            // `handle_delegate` lookup-by-name matches what
            // `tab.rs::open_orchestration_tab` stored in the
            // `TabMembership` / `Tab::Orchestration::name`. Both sides
            // call this loader; doing the normalization here is the one
            // place that keeps the contract consistent.
            for orch in &mut config.orchestrations {
                if orch.name.is_empty() {
                    orch.name = resolve_orchestration_name(&orch.name, dir);
                }
            }
            Ok(Some(config))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ProjectConfigError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_full_config() {
        let toml = r#"
[[modes]]
name = "kubernetes-operations"
shell_init = "devbox shell"

[[modes.panes]]
command = "kubectl get applications -n argocd -w"
name = "ArgoCD Apps"

[[modes.panes]]
command = "kubectl get events -A -w"
name = "Events"

[[modes.rules]]
pattern = "kubectl\\s+.*(describe|explain)"
watch = false

[[modes.rules]]
pattern = "kubectl\\s+.*(get|top|logs)"
watch = true
interval = 2

[[modes.rules]]
pattern = "helm\\s+.*(status|list)"
watch = false
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);

        let mode = &config.modes[0];
        assert_eq!(mode.name, "kubernetes-operations");
        assert_eq!(mode.panes.len(), 2);
        assert_eq!(
            mode.panes[0].command,
            "kubectl get applications -n argocd -w"
        );
        assert_eq!(mode.panes[0].name.as_deref(), Some("ArgoCD Apps"));
        assert_eq!(mode.panes[1].command, "kubectl get events -A -w");
        assert_eq!(mode.panes[1].name.as_deref(), Some("Events"));
        assert_eq!(mode.rules.len(), 3);
        assert_eq!(mode.rules[0].pattern, "kubectl\\s+.*(describe|explain)");
        assert!(!mode.rules[0].watch);
        assert!(mode.rules[0].interval.is_none());
        assert_eq!(mode.rules[1].pattern, "kubectl\\s+.*(get|top|logs)");
        assert!(mode.rules[1].watch);
        assert_eq!(mode.rules[1].interval, Some(2));
        assert!(!mode.rules[2].watch);
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[[modes]]
name = "minimal"

[[modes.panes]]
command = "echo hello"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        let mode = &config.modes[0];
        assert_eq!(mode.name, "minimal");
        assert_eq!(mode.panes.len(), 1);
        assert!(mode.rules.is_empty());
    }

    // PRD #127 M3.1 — `seed_prompt` is an optional mode field: present →
    // parsed, absent → None (existing configs without it keep parsing).
    #[test]
    fn seed_prompt_parses_when_present() {
        let toml = r#"
[[modes]]
name = "seeded"
seed_prompt = "do the thing"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes[0].seed_prompt.as_deref(), Some("do the thing"));
    }

    #[test]
    fn seed_prompt_defaults_to_none_when_absent() {
        let toml = r#"
[[modes]]
name = "plain"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes[0].seed_prompt.is_none());
    }

    #[test]
    fn watch_defaults_to_false() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.rules]]
pattern = "some pattern"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        let rule = &config.modes[0].rules[0];
        assert!(!rule.watch);
        assert!(rule.interval.is_none());
    }

    #[test]
    fn pane_watch_defaults_to_true() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "kubectl get pods"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes[0].panes[0].watch);
    }

    #[test]
    fn pane_watch_can_be_set_to_false() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "kubectl get pods -w"
watch = false
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(!config.modes[0].panes[0].watch);
    }

    #[test]
    fn pane_name_defaults_to_none() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.panes]]
command = "cargo test"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes[0].panes[0].name.is_none());
    }

    #[test]
    fn reactive_panes_defaults_to_two() {
        let toml = r#"
[[modes]]
name = "test"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes[0].reactive_panes, 2);
    }

    #[test]
    fn reactive_panes_configurable() {
        let toml = r#"
[[modes]]
name = "test"
reactive_panes = 4
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes[0].reactive_panes, 4);
    }

    #[test]
    fn parse_full_orchestration_config() {
        let toml = r#"
[[orchestrations]]
name = "code-review"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = "You coordinate the team."

[[orchestrations.roles]]
name = "coder"
command = "claude --model sonnet"
description = "Implements code changes"
prompt_template = "Always run cargo test before finishing."
clear = false
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.orchestrations.len(), 1);
        let orch = &config.orchestrations[0];
        assert_eq!(orch.name, "code-review");
        assert_eq!(orch.roles.len(), 2);
        assert_eq!(orch.roles[0].name, "orchestrator");
        assert_eq!(orch.roles[0].command, "claude");
        assert!(orch.roles[0].start);
        assert_eq!(
            orch.roles[0].prompt_template.as_deref(),
            Some("You coordinate the team.")
        );
        assert!(orch.roles[0].description.is_none());
        assert!(orch.roles[0].clear); // default true
        assert_eq!(orch.roles[1].name, "coder");
        assert!(!orch.roles[1].start);
        assert_eq!(
            orch.roles[1].description.as_deref(),
            Some("Implements code changes")
        );
        assert!(!orch.roles[1].clear); // explicitly false
    }

    #[test]
    fn parse_orchestration_alongside_modes() {
        let toml = r#"
[[modes]]
name = "dev"

[[modes.panes]]
command = "echo hi"

[[orchestrations]]
name = "review"

[[orchestrations.roles]]
name = "writer"
command = "claude"
start = true

[[orchestrations.roles]]
name = "reviewer"
command = "claude"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);
        assert_eq!(config.orchestrations.len(), 1);
    }

    #[test]
    fn orchestration_clear_defaults_to_true() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.orchestrations[0].roles[0].clear);
    }

    #[test]
    fn orchestration_description_defaults_to_none() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.orchestrations[0].roles[0].description.is_none());
    }

    #[test]
    fn orchestration_prompt_template_defaults_to_none() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.orchestrations[0].roles[0].prompt_template.is_none());
    }

    // ------------------------------------------------------------
    // PRD #111: synthesize_from_bucket_metadata
    // ------------------------------------------------------------

    #[test]
    fn synthesize_uses_provided_orchestration_name() {
        let slots = vec![SynthesisRoleSlot {
            role_index: 0,
            role_name: "orchestrator".into(),
            is_start_role: true,
        }];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("code-review", &slots);
        assert_eq!(cfg.name, "code-review");
    }

    #[test]
    fn synthesize_role_count_matches_max_index_plus_one() {
        // role_index 2 → roles.len() must be 3 so the open-tab length
        // check passes even when role 1 is a dead slot.
        let slots = vec![
            SynthesisRoleSlot {
                role_index: 0,
                role_name: "orchestrator".into(),
                is_start_role: true,
            },
            SynthesisRoleSlot {
                role_index: 2,
                role_name: "reviewer".into(),
                is_start_role: false,
            },
        ];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("review", &slots);
        assert_eq!(cfg.roles.len(), 3);
        assert_eq!(cfg.roles[0].name, "orchestrator");
        // Missing slot at index 1 → placeholder name.
        assert_eq!(cfg.roles[1].name, "role-1");
        assert_eq!(cfg.roles[2].name, "reviewer");
    }

    #[test]
    fn synthesize_marks_start_role_from_metadata() {
        let slots = vec![
            SynthesisRoleSlot {
                role_index: 0,
                role_name: "worker".into(),
                is_start_role: false,
            },
            SynthesisRoleSlot {
                role_index: 1,
                role_name: "orchestrator".into(),
                is_start_role: true,
            },
        ];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &slots);
        assert!(!cfg.roles[0].start);
        assert!(cfg.roles[1].start);
        // `roles.iter().position(|r| r.start)` should resolve to 1.
        assert_eq!(cfg.roles.iter().position(|r| r.start), Some(1));
    }

    #[test]
    fn synthesize_leaves_display_fields_at_defaults() {
        let slots = vec![SynthesisRoleSlot {
            role_index: 0,
            role_name: "orchestrator".into(),
            is_start_role: true,
        }];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &slots);
        let role = &cfg.roles[0];
        assert_eq!(role.command, "");
        assert!(role.description.is_none());
        assert!(role.prompt_template.is_none());
        // `clear` default mirrors the toml loader's default (true).
        assert!(role.clear);
    }

    #[test]
    fn synthesize_handles_empty_role_name_via_placeholder() {
        // Older daemons predating the inline role_name field may emit an
        // empty role_name; synthesize must still produce a usable label.
        let slots = vec![SynthesisRoleSlot {
            role_index: 0,
            role_name: String::new(),
            is_start_role: true,
        }];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &slots);
        assert_eq!(cfg.roles[0].name, "role-0");
        assert!(cfg.roles[0].start);
    }

    #[test]
    fn synthesize_empty_slots_yields_empty_roles() {
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &[]);
        assert!(cfg.roles.is_empty());
        assert_eq!(cfg.name, "o");
    }

    // PRD #120 H1 (defense in depth): a pathological role_index that slips past
    // the wire-boundary validators must NOT size a giant placeholder vec
    // (`usize::MAX + 1` panics in debug; a billion-element vec OOMs). The clamp
    // bounds role_count and the bounds-checked slot loop skips the over-cap
    // index without panicking.
    #[test]
    fn synthesize_clamps_pathological_role_index() {
        let slots = vec![SynthesisRoleSlot {
            role_index: usize::MAX,
            role_name: "rogue".into(),
            is_start_role: false,
        }];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &slots);
        assert!(
            cfg.roles.len() <= crate::agent_pty::ORCHESTRATION_ROLE_INDEX_MAX + 1,
            "role_count must be clamped, got {}",
            cfg.roles.len()
        );
    }

    #[test]
    fn synthesize_first_wins_on_duplicate_role_index() {
        // PRD #111 reviewer S2: synthesis must agree with the
        // hydration loop's first-wins tie-break for duplicate
        // role_index. The daemon is not supposed to emit duplicates,
        // but if it does, the synthesised config's role.name and
        // role.start must come from the same slot whose pane survives
        // the hydration de-dup (`src/ui.rs::hydration`) — otherwise the
        // tab label and the live pane come from different bucket
        // entries.
        let slots = vec![
            SynthesisRoleSlot {
                role_index: 0,
                role_name: "first".into(),
                is_start_role: true,
            },
            SynthesisRoleSlot {
                role_index: 0,
                role_name: "second".into(),
                is_start_role: false,
            },
        ];
        let cfg = OrchestrationConfig::synthesize_from_bucket_metadata("o", &slots);
        assert_eq!(cfg.roles.len(), 1);
        assert_eq!(
            cfg.roles[0].name, "first",
            "first slot must win the role_name tie-break"
        );
        assert!(
            cfg.roles[0].start,
            "first slot must win the is_start_role tie-break"
        );
    }

    #[test]
    fn orchestration_role_start_defaults_to_false() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "worker"
command = "claude"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(!config.orchestrations[0].roles[0].start);
    }

    #[test]
    fn modes_only_config_still_works() {
        let toml = r#"
[[modes]]
name = "dev"

[[modes.panes]]
command = "echo hi"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.modes.len(), 1);
        assert!(config.orchestrations.is_empty());
    }

    #[test]
    fn orchestrations_only_config_works() {
        let toml = r#"
[[orchestrations]]
name = "test"

[[orchestrations.roles]]
name = "a"
command = "claude"
start = true

[[orchestrations.roles]]
name = "b"
command = "claude"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert!(config.modes.is_empty());
        assert_eq!(config.orchestrations.len(), 1);
    }

    #[test]
    fn missing_required_pattern_is_error() {
        let toml = r#"
[[modes]]
name = "test"

[[modes.rules]]
watch = true
"#;
        let result: Result<ProjectConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    // ---- Issue #308: the config-declared agent type -------------------------
    //
    // Pure-data tests over the parse + resolve path. The launch-shape and badge
    // consequences of what these resolve to belong to the PTY-registry tier
    // (`codex/spawn/009`–`012`); what is pinned here is the resolution contract
    // every one of those seams reads.

    fn role_of(toml_src: &str) -> OrchestrationRoleConfig {
        let config: ProjectConfig = toml::from_str(toml_src).expect("role config parses");
        config
            .orchestrations
            .into_iter()
            .next()
            .expect("one orchestration")
            .roles
            .into_iter()
            .next()
            .expect("one role")
    }

    fn mode_of(toml_src: &str) -> ModeConfig {
        let config: ProjectConfig = toml::from_str(toml_src).expect("mode config parses");
        config.modes.into_iter().next().expect("one mode")
    }

    fn role_with(agent_line: &str) -> OrchestrationRoleConfig {
        role_of(&format!(
            r#"
[[orchestrations]]
name = "declared"

[[orchestrations.roles]]
name = "worker"
command = "devbox run codex-big"
{agent_line}
"#
        ))
    }

    /// A declared name the registry knows resolves to that agent — for a role
    /// command (`devbox run codex-big`) that derives nothing at all, which is
    /// the entire reason the key exists.
    #[test]
    fn declared_role_agent_resolves_through_the_registry() {
        let role = role_with(r#"agent = "codex""#);
        assert_eq!(role.agent.as_deref(), Some("codex"));
        assert_eq!(role.declared_agent_type(), Some(AgentType::Codex));
        assert_eq!(
            role.resolved_agent_type(),
            Some(AgentType::Codex),
            "a launcher command derives nothing, so the declaration is the only answer"
        );
    }

    /// The compatibility case, and the one every config written before this key
    /// existed takes: no `agent` line at all behaves exactly as before —
    /// derivation from the command, and nothing else.
    #[test]
    fn a_role_without_the_key_is_unchanged() {
        let absent = role_with("");
        assert_eq!(absent.agent, None);
        assert_eq!(absent.declared_agent_type(), None);
        assert_eq!(
            absent.resolved_agent_type(),
            AgentType::from_command(Some("devbox run codex-big")),
            "with nothing declared the answer must be the derivation, verbatim"
        );

        let inferable = role_of(
            r#"
[[orchestrations]]
name = "plain"

[[orchestrations.roles]]
name = "worker"
command = "claude --model haiku"
"#,
        );
        assert_eq!(inferable.declared_agent_type(), None);
        assert_eq!(
            inferable.resolved_agent_type(),
            Some(AgentType::ClaudeCode),
            "an undeclared but inferable command still derives its type"
        );
    }

    /// An unrecognized name is a DECLARATION that resolves to the neutral
    /// `AgentType::None` — never a fallback to guessing from the command. Same
    /// rule `wrap --agent <typo>` applies, and the reason it matters is the
    /// second half: the declaration stands even when the command would have
    /// derived something, so a typo produces a visibly agent-less pane instead
    /// of a plausible wrong one.
    #[test]
    fn an_unrecognized_declared_name_never_guesses() {
        let role = role_with(r#"agent = "nonsense""#);
        assert_eq!(role.declared_agent_type(), Some(AgentType::None));
        assert_eq!(role.resolved_agent_type(), Some(AgentType::None));

        let over_inferable = role_of(
            r#"
[[orchestrations]]
name = "typo"

[[orchestrations.roles]]
name = "worker"
command = "claude --model haiku"
agent = "codx"
"#,
        );
        assert_eq!(
            over_inferable.resolved_agent_type(),
            Some(AgentType::None),
            "a declaration outranks derivation even when it resolves to nothing — \
             silently overruling the user with a guess is what this must not do"
        );
    }

    /// Whitespace handling. An empty or blank value reads as UNSET (fall back to
    /// the command) rather than as an explicit "no agent", so `agent = ""` can
    /// never strip the wrapper off an otherwise perfectly inferable command; a
    /// value with stray spaces around a real name still resolves.
    #[test]
    fn blank_declarations_are_unset_and_padded_ones_still_resolve() {
        for blank in ["", "   ", "\t"] {
            let role = role_with(&format!(r#"agent = "{blank}""#));
            assert_eq!(
                role.declared_agent_type(),
                None,
                "a blank agent value must read as unset, not as an explicit no-agent"
            );
        }
        assert_eq!(
            role_with(r#"agent = "  codex  ""#).declared_agent_type(),
            Some(AgentType::Codex)
        );
    }

    /// Matching is by detection basename and is case-SENSITIVE, because this
    /// resolves through the same `agent_registry::resolve_declared_agent` that
    /// backs `wrap --agent`. Pinned so the two surfaces cannot be "fixed" apart.
    #[test]
    fn declared_names_resolve_exactly_as_wrap_agent_does() {
        for name in [
            "codex", "claude", "opencode", "pi", "Codex", "CLAUDE", "nope",
        ] {
            assert_eq!(
                role_with(&format!(r#"agent = "{name}""#)).declared_agent_type(),
                Some(crate::agent_registry::resolve_declared_agent(name)),
                "`agent = \"{name}\"` must resolve identically to `wrap --agent {name}`"
            );
        }
    }

    /// The mode surface carries the same key with the same rules — but on
    /// `[[modes]]`, whose agent pane command is typed in the new-pane form, so
    /// the resolution takes that command as an argument.
    #[test]
    fn declared_mode_agent_applies_to_the_typed_agent_pane_command() {
        let declared = mode_of(
            r#"
[[modes]]
name = "declared-codex-mode"
agent = "codex"
reactive_panes = 0
"#,
        );
        assert_eq!(declared.declared_agent_type(), Some(AgentType::Codex));
        assert_eq!(
            declared.resolved_agent_type("devbox run codex-big"),
            Some(AgentType::Codex),
            "the declaration is what identifies a launcher the form typed in"
        );

        let plain = mode_of(
            r#"
[[modes]]
name = "plain"
reactive_panes = 0
"#,
        );
        assert_eq!(plain.declared_agent_type(), None);
        assert_eq!(
            plain.resolved_agent_type("devbox run codex-big"),
            None,
            "an undeclared mode is unchanged: a launcher still resolves to nothing"
        );
        assert_eq!(
            plain.resolved_agent_type("codex"),
            Some(AgentType::Codex),
            "…and an inferable command still derives"
        );
    }

    /// Issue #308 follow-up: a `.dot-agent-deck.toml` that merely FAILS TO
    /// PARSE must not be able to paint the terminal `dot-agent-deck validate`
    /// prints into. `toml` renders the offending source line verbatim, so this
    /// goes through the real loader rather than a hand-built error — the raw
    /// ESC has to survive `toml`'s own rendering to be worth escaping.
    #[test]
    fn a_parse_error_cannot_smuggle_escapes_out_of_the_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[[modes]]\nname = \"ok\"\nbogus = \u{1b}[31mPWNED\u{1b}[0m\n",
        )
        .expect("write config");

        let err = load_project_config(dir.path()).expect_err("the config does not parse");
        let rendered = err.to_string();

        assert!(
            !rendered.contains('\u{1b}'),
            "no ESC reaches the terminal; got {rendered:?}"
        );
        assert!(
            rendered.contains("\\u{1b}[31mPWNED"),
            "it is shown as text, so the evidence survives; got {rendered:?}"
        );
        assert!(
            rendered.contains("TOML parse error"),
            "and it is still a toml diagnostic; got {rendered:?}"
        );
    }

    /// Issue #308 follow-up: the other half of the same seam — an ORDINARY
    /// syntax error must stay exactly as readable as it was. The gutter frame
    /// is the whole value of a `toml` error, so its own newlines survive.
    #[test]
    fn an_ordinary_syntax_error_keeps_its_multi_line_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            "[[modes]]\nname = \"ok\"\nbogus\n",
        )
        .expect("write config");

        let rendered = load_project_config(dir.path())
            .expect_err("the config does not parse")
            .to_string();

        assert!(
            rendered.lines().count() >= 4,
            "the frame keeps its line structure; got {rendered:?}"
        );
        assert!(
            !rendered.contains("\\n"),
            "the newlines are real, not escaped away; got {rendered:?}"
        );
        assert!(
            rendered.contains("3 | bogus"),
            "the offending line is still quoted under its gutter; got {rendered:?}"
        );
    }
}
