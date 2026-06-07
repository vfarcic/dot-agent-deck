use std::path::Path;

use serde::Deserialize;

pub const CONFIG_FILE_NAME: &str = ".dot-agent-deck.toml";

#[derive(Debug, thiserror::Error)]
pub enum ProjectConfigError {
    #[error("Failed to read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("Failed to parse {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub modes: Vec<ModeConfig>,
    #[serde(default)]
    pub orchestrations: Vec<OrchestrationConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeConfig {
    pub name: String,
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

#[derive(Debug, Clone, Deserialize)]
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
        let role_count = if slots.is_empty() { 0 } else { max_index + 1 };
        let mut roles: Vec<OrchestrationRoleConfig> = (0..role_count)
            .map(|i| OrchestrationRoleConfig {
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

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestrationRoleConfig {
    pub name: String,
    pub command: String,
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
}
