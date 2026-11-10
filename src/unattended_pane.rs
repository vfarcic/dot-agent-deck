//! Issue #2094: the settings the deck gives an agent in a pane nobody types
//! into.
//!
//! Claude Code generates *prompt suggestions* — a guessed next prompt, shown
//! greyed out in its input box — with an extra model request after each turn.
//! Telemetry round 1 (#1980) measured those requests at 3 % of Claude Code's
//! estimated cost, and in a dispatched unit's pane or an orchestration worker's
//! pane there is no person to accept one. So those panes start with
//! `CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=false`; the panes where a person types
//! (an orchestrator, a dispatcher, a plain pane the user opened) are left as
//! Claude Code configures them.
//!
//! The switch is per process: Claude Code reads the variable before its
//! `promptSuggestionEnabled` setting (checked against 2.1.322), so the user's
//! own settings file is never written. Other agents read nothing under this
//! name, which is why it is set by pane rather than by agent type: a role
//! started through a launcher (`devbox run -- claude`) is covered too.

use crate::agent_pty::TabMembership;
use crate::hook_provenance::InstanceOrigin;

/// Claude Code's environment switch for prompt suggestions. It is tri-state:
/// unset defers to the `promptSuggestionEnabled` setting, a false value turns
/// them off whatever that setting says.
pub const CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: &str = "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION";

/// Whether a pane spawned with this membership and origin has no person typing
/// into it: an orchestration role that is not the orchestrator seat (whether
/// the orchestration was dispatched or opened with `Ctrl+N`), or a dispatched
/// single-agent unit.
pub fn is_unattended(membership: Option<&TabMembership>, origin: Option<InstanceOrigin>) -> bool {
    match membership {
        Some(TabMembership::Orchestration { is_start_role, .. }) => !is_start_role,
        Some(TabMembership::Mode { .. }) => false,
        None => origin == Some(InstanceOrigin::Dispatched),
    }
}

/// Add the unattended-pane settings to `env` when the pane is unattended. A
/// value the caller already put in `env` is kept, so a respawn that replays a
/// generation's env does not duplicate it and an explicit caller value wins.
pub fn apply(
    env: &mut Vec<(String, String)>,
    membership: Option<&TabMembership>,
    origin: Option<InstanceOrigin>,
) {
    if !is_unattended(membership, origin) {
        return;
    }
    if env
        .iter()
        .any(|(k, _)| k == CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION)
    {
        return;
    }
    env.push((
        CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION.to_string(),
        "false".to_string(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(is_start_role: bool) -> TabMembership {
        TabMembership::Orchestration {
            name: "team".to_string(),
            role_index: if is_start_role { 0 } else { 1 },
            role_name: if is_start_role {
                "orchestrator"
            } else {
                "coder"
            }
            .to_string(),
            is_start_role,
            orchestration_cwd: None,
            display_title: None,
            orchestration_id: Some("o-1".to_string()),
        }
    }

    fn suggestion(env: &[(String, String)]) -> Option<&str> {
        env.iter()
            .find(|(k, _)| k == CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION)
            .map(|(_, v)| v.as_str())
    }

    /// Scenario: a worker role, dispatched or opened with Ctrl+N, and a
    /// dispatched single-agent unit start with prompt suggestions off.
    #[test]
    fn unattended_panes_turn_prompt_suggestions_off() {
        for (membership, origin) in [
            (Some(role(false)), Some(InstanceOrigin::Dispatched)),
            (Some(role(false)), Some(InstanceOrigin::Direct)),
            (Some(role(false)), None),
            (None, Some(InstanceOrigin::Dispatched)),
        ] {
            let mut env = Vec::new();
            apply(&mut env, membership.as_ref(), origin);
            assert_eq!(
                suggestion(&env),
                Some("false"),
                "{membership:?} / {origin:?}"
            );
        }
    }

    /// Scenario: the orchestrator seat, a plain pane, a scheduled pane and a
    /// mode pane are left as Claude Code configures them.
    #[test]
    fn panes_a_person_types_into_are_left_alone() {
        for (membership, origin) in [
            (Some(role(true)), Some(InstanceOrigin::Dispatched)),
            (Some(role(true)), Some(InstanceOrigin::Direct)),
            (None, None),
            (None, Some(InstanceOrigin::Direct)),
            (
                Some(TabMembership::Mode {
                    name: "dev".to_string(),
                }),
                Some(InstanceOrigin::Dispatched),
            ),
        ] {
            let mut env = Vec::new();
            apply(&mut env, membership.as_ref(), origin);
            assert_eq!(suggestion(&env), None, "{membership:?} / {origin:?}");
        }
    }

    /// Scenario: a respawn replays the previous generation's env; the value is
    /// not duplicated, and a value the caller chose is kept.
    #[test]
    fn a_value_already_in_the_env_is_kept() {
        let mut env = vec![(
            CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION.to_string(),
            "true".to_string(),
        )];
        apply(&mut env, Some(&role(false)), None);
        assert_eq!(env.len(), 1);
        assert_eq!(suggestion(&env), Some("true"));
    }
}
