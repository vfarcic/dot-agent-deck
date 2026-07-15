//! PRD #20 M2 — the compiled-in agent registry + integration-strategy seam.
//!
//! Before this module, everything the deck knew about a specific agent lived in
//! scattered `match AgentType` arms: detection in [`crate::event::AgentType::from_command`],
//! the human label in the `Display` impl (`src/ui.rs`), the default authoring
//! command as a lone `const` (`src/ui.rs`), and the install/materialize dispatch
//! spread across `main.rs` and `agent_pty.rs`. Adding an agent meant touching
//! every one of those sites.
//!
//! This module centralises that per-agent data into **one cohesive
//! [`AgentSpec`] entry per agent** and names a small finite set of integration
//! [`IntegrationStrategy`] mechanisms. The agent identity stays a typed
//! [`crate::event::AgentType`] enum keyed into the registry — runtime/user
//! extensibility is an explicit non-goal (every new agent ships in a release
//! anyway), so a recompile-per-agent is acceptable and the win is
//! maintainability, not destructuring.
//!
//! This is a **behaviour-preserving** move for the shipped agents (Claude Code,
//! OpenCode, Pi): the scattered sites now READ from here instead of hardcoding,
//! and the existing test suite passes unchanged as the regression proof. The
//! badge colour field is populated now even though rendering it on cards is a
//! later milestone — the registry is meant to be the single source of truth per
//! the PRD success criteria.

use ratatui::style::Color;

use crate::event::AgentType;

/// The finite set of mechanisms by which an agent's activity reaches the deck.
///
/// The two originally-shipped agents already used two different mechanisms
/// (native hooks vs. a plugin), and Pi added a third (a bundled extension),
/// which is precisely why this layer is inherently code rather than data:
/// adding an agent that reuses an existing strategy is a registry entry (+
/// release), while a genuinely new mechanism is a one-time strategy
/// implementation, then a registry entry thereafter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationStrategy {
    /// Native hook scripts installed into the agent's own config
    /// (Claude Code — `src/hooks_manage.rs`).
    NativeHooks,
    /// A JS plugin materialized into the agent's plugin directory
    /// (OpenCode — `src/opencode_manage.rs`).
    Plugin,
    /// A bundled extension materialized into the agent's HOME
    /// (Pi — `src/orchestrator_ext.rs`).
    Extension,
    /// A stdout wrapper (`dot-agent-deck wrap`) that intercepts the agent's
    /// stdio to synthesize events. **Reserved** for the coming Codex/Gemini
    /// work (later PRD #20 milestones): the variant is defined now so the
    /// registry shape is stable, but no shipped agent uses it yet and nothing
    /// dispatches on it.
    Wrapper,
}

/// PRD #20 finding #15: an integration-hook handler (install / uninstall) —
/// `Ok(())` on success, `Err(message)` on a reported failure.
pub type HookFn = fn() -> Result<(), String>;

/// PRD #20 finding #15: a spawn-time materialize handler (the `Extension`
/// strategy). Receives the spawn env so it can honor `HOME` / deck env vars.
pub type MaterializeFn = fn(&[(String, String)]);

/// One cohesive registry entry per agent — the single place per-agent data
/// lives (PRD #20 success criteria).
#[derive(Debug)]
pub struct AgentSpec {
    /// The typed identity this entry is keyed by.
    pub agent_type: AgentType,
    /// Human-facing label shown on cards / in the `Display` impl.
    pub label: &'static str,
    /// Binary basenames that resolve to this agent in
    /// [`crate::event::AgentType::from_command`]. Empty for the neutral
    /// [`NONE`] placeholder (it is never detected from a command).
    pub detect_basenames: &'static [&'static str],
    /// The canonical command that launches this agent, if it has one. `None`
    /// for the neutral placeholder.
    pub default_command: Option<&'static str>,
    /// Which integration mechanism carries this agent's events to the deck.
    /// `None` for the neutral placeholder (not a real agent).
    pub strategy: Option<IntegrationStrategy>,
    /// Per-agent badge colour. Populated now as the single source of truth even
    /// though rendering coloured badges on cards is a later PRD #20 milestone.
    /// A named ANSI colour only (no absolute `Color::Rgb`), matching the
    /// palette policy (`src/palette.rs`) so terminal themes can remap it.
    pub badge_color: Color,
    /// PRD #20 finding #15: this agent's OWN integration handlers, so strategy
    /// dispatch resolves from the SPEC rather than a hardcoded incumbent module
    /// keyed by the [`IntegrationStrategy`] enum. Before this, every `NativeHooks`
    /// entry ran Claude's installer, every `Plugin` ran OpenCode's, and every
    /// `Extension` materialized Pi's — so a FUTURE agent reusing one of those
    /// strategies would run another agent's implementation. With the handler on
    /// the spec, `main.rs` / `agent_pty.rs` call `spec(x).hook_install` etc. and
    /// a new agent slots in its own handlers here. `None` where the agent's
    /// strategy has no such action (a Wrapper agent has no hook installer or
    /// extension materialize; the neutral placeholder has none at all).
    pub hook_install: Option<HookFn>,
    pub hook_uninstall: Option<HookFn>,
    /// Materialize a bundled artifact into the agent's HOME just before spawn
    /// (the `Extension` strategy).
    pub materialize: Option<MaterializeFn>,
    /// PRD #20 R20-010: this agent's OWN startup auto-install action — the
    /// silent, best-effort install the TUI runs at launch for every shipped
    /// agent. Before this, startup dispatched by matching the reusable
    /// [`IntegrationStrategy`] enum to a hardcoded incumbent
    /// (`NativeHooks` → Claude's installer, `Plugin` → OpenCode's), so a FUTURE
    /// agent reusing one of those strategies would run another agent's
    /// installer. With the action on the spec, `main.rs` iterates [`ALL`] and
    /// calls `spec.startup_auto_install` directly, and a new agent slots in its
    /// own installer here. `None` where the agent has no startup install step —
    /// a spawn-time `Extension` (Pi materializes at spawn), a `Wrapper` (Codex
    /// synthesizes events from stdout), or the neutral placeholder.
    pub startup_auto_install: Option<fn()>,
}

// PRD #20 finding #15: per-agent adapters that normalize each incumbent
// module's signature to the spec's handler shape. Keeping them here (as the
// values the statics point at) is what makes dispatch spec-resolved.
fn claude_install() -> Result<(), String> {
    crate::hooks_manage::install();
    Ok(())
}
fn claude_uninstall() -> Result<(), String> {
    crate::hooks_manage::uninstall();
    Ok(())
}
fn opencode_install() -> Result<(), String> {
    crate::opencode_manage::install().map_err(|e| e.to_string())
}
fn opencode_uninstall() -> Result<(), String> {
    crate::opencode_manage::uninstall().map_err(|e| e.to_string())
}
fn pi_materialize(env: &[(String, String)]) {
    crate::orchestrator_ext::auto_materialize(env);
}

/// Claude Code — native-hooks strategy (shipped).
pub static CLAUDE_CODE: AgentSpec = AgentSpec {
    agent_type: AgentType::ClaudeCode,
    label: "ClaudeCode",
    detect_basenames: &["claude"],
    default_command: Some("claude"),
    strategy: Some(IntegrationStrategy::NativeHooks),
    badge_color: Color::LightMagenta,
    hook_install: Some(claude_install),
    hook_uninstall: Some(claude_uninstall),
    materialize: None,
    startup_auto_install: Some(crate::hooks_manage::auto_install),
};

/// OpenCode — plugin strategy (shipped).
pub static OPEN_CODE: AgentSpec = AgentSpec {
    agent_type: AgentType::OpenCode,
    label: "OpenCode",
    detect_basenames: &["opencode"],
    default_command: Some("opencode"),
    strategy: Some(IntegrationStrategy::Plugin),
    badge_color: Color::LightGreen,
    hook_install: Some(opencode_install),
    hook_uninstall: Some(opencode_uninstall),
    materialize: None,
    startup_auto_install: Some(crate::opencode_manage::auto_install),
};

/// Pi — bundled-extension strategy (shipped, PRD #201).
pub static PI: AgentSpec = AgentSpec {
    agent_type: AgentType::Pi,
    label: "Pi",
    detect_basenames: &["pi"],
    default_command: Some("pi"),
    strategy: Some(IntegrationStrategy::Extension),
    badge_color: Color::LightCyan,
    hook_install: None,
    hook_uninstall: None,
    materialize: Some(pi_materialize),
    // Pi materializes its extension at SPAWN time, not startup.
    startup_auto_install: None,
};

/// Codex — stdout-wrapper strategy (PRD #20 M7). The first agent to use the
/// [`IntegrationStrategy::Wrapper`] mechanism: `dot-agent-deck wrap -- codex …`
/// tees Codex's stdout through pattern detection into `AgentEvent`s. Its badge
/// colour is a distinct named ANSI colour (LightYellow) — not reused by Claude
/// (LightMagenta), OpenCode (LightGreen), or Pi (LightCyan), and never the
/// neutral [`NONE`] DarkGray.
pub static CODEX: AgentSpec = AgentSpec {
    agent_type: AgentType::Codex,
    label: "Codex",
    detect_basenames: &["codex"],
    default_command: Some("codex"),
    strategy: Some(IntegrationStrategy::Wrapper),
    badge_color: Color::LightYellow,
    // Wrapper agents synthesize events from stdout — no hook install, no
    // extension materialize (the `dot-agent-deck wrap` seam does the work).
    hook_install: None,
    hook_uninstall: None,
    materialize: None,
    // Wrapper agents have no startup install step.
    startup_auto_install: None,
};

/// Neutral entry for the "no recognized agent" placeholder. Not a real agent:
/// it has no detection basenames, no default command, and no integration
/// strategy. It exists so registry lookups ([`spec`]) are total — the `Display`
/// label path still resolves through here — and so unknown/`None` gets a
/// deliberate neutral badge colour rather than an accidental one.
pub static NONE: AgentSpec = AgentSpec {
    agent_type: AgentType::None,
    label: "No agent",
    detect_basenames: &[],
    default_command: None,
    strategy: None,
    badge_color: Color::DarkGray,
    hook_install: None,
    hook_uninstall: None,
    materialize: None,
    startup_auto_install: None,
};

/// All SHIPPED, detectable agents, in a stable order. Excludes the neutral
/// [`NONE`] placeholder — it is not a detectable agent and has no strategy to
/// dispatch. Detection and startup auto-install iterate this slice.
pub static ALL: &[&AgentSpec] = &[&CLAUDE_CODE, &OPEN_CODE, &PI, &CODEX];

/// The registry entry for a given agent type. Total: every [`AgentType`]
/// variant — including the neutral [`AgentType::None`] — maps to an entry, so
/// callers never have to special-case the placeholder.
pub fn spec(agent_type: &AgentType) -> &'static AgentSpec {
    match agent_type {
        AgentType::ClaudeCode => &CLAUDE_CODE,
        AgentType::OpenCode => &OPEN_CODE,
        AgentType::Pi => &PI,
        AgentType::Codex => &CODEX,
        AgentType::None => &NONE,
    }
}

/// Resolve a binary basename to its agent type, or `None` if no shipped agent
/// claims it. Backs [`crate::event::AgentType::from_command`]: an unrecognized
/// basename yields `None` (the daemon then stores "type not known" rather than
/// misclassifying), exactly as the hand-written `match` did before this move.
pub fn detect_from_basename(basename: &str) -> Option<AgentType> {
    ALL.iter()
        .find(|spec| spec.detect_basenames.contains(&basename))
        .map(|spec| spec.agent_type.clone())
}

/// PRD #20 M9: resolve a `type:<alias>` dashboard-filter token to an agent
/// type, matching case-insensitively against either the agent's human [`label`]
/// (e.g. `type:codex`, `type:ClaudeCode`) or any of its detection basenames
/// (e.g. `type:claude`). Returns `None` for an unrecognized or empty alias so
/// the `/` filter (`src/ui.rs`) can treat `type:bogus` as "matches nothing".
///
/// Driven by [`ALL`] so every shipped agent is filterable and a future agent
/// needs no new filter code — the neutral [`NONE`] placeholder is excluded (it
/// is not a real, filterable agent).
///
/// [`label`]: AgentSpec::label
pub fn resolve_type_alias(alias: &str) -> Option<AgentType> {
    let alias = alias.trim();
    if alias.is_empty() {
        return None;
    }
    ALL.iter()
        .find(|spec| {
            spec.label.eq_ignore_ascii_case(alias)
                || spec
                    .detect_basenames
                    .iter()
                    .any(|basename| basename.eq_ignore_ascii_case(alias))
        })
        .map(|spec| spec.agent_type.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-data registry lookups — plain `#[test]` unit tests (no `#[spec]` /
    // CATALOG reproducer needed; these assert compiled-in data, not runtime
    // TUI behaviour).

    /// Every shipped agent's registry label equals what the `Display` impl
    /// rendered before centralisation, and the neutral placeholder is still
    /// "No agent". These strings are user-visible in card titles, so the move
    /// must not change them.
    #[test]
    fn labels_match_prior_display_strings() {
        assert_eq!(spec(&AgentType::ClaudeCode).label, "ClaudeCode");
        assert_eq!(spec(&AgentType::OpenCode).label, "OpenCode");
        assert_eq!(spec(&AgentType::Pi).label, "Pi");
        assert_eq!(spec(&AgentType::None).label, "No agent");

        // And the `Display` impl (src/ui.rs) now reads through the registry, so
        // formatting an AgentType yields the same label.
        assert_eq!(format!("{}", AgentType::ClaudeCode), "ClaudeCode");
        assert_eq!(format!("{}", AgentType::OpenCode), "OpenCode");
        assert_eq!(format!("{}", AgentType::Pi), "Pi");
        assert_eq!(format!("{}", AgentType::None), "No agent");
    }

    /// Detection through the registry reproduces the prior `from_command`
    /// mapping exactly: the three shipped binaries resolve to their types and
    /// everything else is unrecognized.
    #[test]
    fn detect_from_basename_matches_prior_mapping() {
        assert_eq!(detect_from_basename("claude"), Some(AgentType::ClaudeCode));
        assert_eq!(detect_from_basename("opencode"), Some(AgentType::OpenCode));
        assert_eq!(detect_from_basename("pi"), Some(AgentType::Pi));
        assert_eq!(detect_from_basename("sh"), None);
        assert_eq!(detect_from_basename("vim"), None);
        assert_eq!(detect_from_basename(""), None);
    }

    /// The public `from_command` entry point (event.rs) still infers the type
    /// from a full spawn command via the registry — same binary/path/arg
    /// handling as before.
    #[test]
    fn from_command_routes_through_registry() {
        assert_eq!(
            AgentType::from_command(Some("claude --dangerously-skip-permissions")),
            Some(AgentType::ClaudeCode)
        );
        assert_eq!(
            AgentType::from_command(Some("/usr/local/bin/pi run")),
            Some(AgentType::Pi)
        );
        assert_eq!(AgentType::from_command(Some("bash")), None);
    }

    /// PRD #20 R20-010: startup auto-install is resolved PER SPEC, not by
    /// mapping the reusable `IntegrationStrategy` enum to a hardcoded incumbent.
    /// The two agents with a startup install step (Claude native hooks, OpenCode
    /// plugin) carry an action; the spawn-time `Extension` (Pi), the `Wrapper`
    /// (Codex), and the neutral placeholder carry `None` — so a future agent
    /// reusing `NativeHooks`/`Plugin` runs ITS OWN installer, never another
    /// agent's.
    #[test]
    fn startup_auto_install_is_resolved_per_spec() {
        assert!(
            spec(&AgentType::ClaudeCode).startup_auto_install.is_some(),
            "Claude installs its native hooks at startup"
        );
        assert!(
            spec(&AgentType::OpenCode).startup_auto_install.is_some(),
            "OpenCode installs its plugin at startup"
        );
        assert!(
            spec(&AgentType::Pi).startup_auto_install.is_none(),
            "Pi materializes its extension at spawn time, not startup"
        );
        assert!(
            spec(&AgentType::Codex).startup_auto_install.is_none(),
            "Codex is a stdout wrapper — no startup install step"
        );
        assert!(
            spec(&AgentType::None).startup_auto_install.is_none(),
            "the neutral placeholder has no startup install step"
        );
    }

    /// Default commands match the prior per-agent launch commands; the neutral
    /// placeholder has none.
    #[test]
    fn default_commands_match_prior_behaviour() {
        assert_eq!(spec(&AgentType::ClaudeCode).default_command, Some("claude"));
        assert_eq!(spec(&AgentType::OpenCode).default_command, Some("opencode"));
        assert_eq!(spec(&AgentType::Pi).default_command, Some("pi"));
        assert_eq!(spec(&AgentType::None).default_command, None);
    }

    /// Each shipped agent names the integration mechanism it actually used
    /// before this move: Claude → native hooks, OpenCode → plugin, Pi →
    /// bundled extension. The neutral placeholder has no strategy.
    #[test]
    fn shipped_agents_map_to_expected_strategy() {
        assert_eq!(
            spec(&AgentType::ClaudeCode).strategy,
            Some(IntegrationStrategy::NativeHooks)
        );
        assert_eq!(
            spec(&AgentType::OpenCode).strategy,
            Some(IntegrationStrategy::Plugin)
        );
        assert_eq!(
            spec(&AgentType::Pi).strategy,
            Some(IntegrationStrategy::Extension)
        );
        assert_eq!(spec(&AgentType::None).strategy, None);
    }

    /// PRD #20 M7: Codex is the wrapper-strategy agent. It is the ONLY shipped
    /// agent that uses [`IntegrationStrategy::Wrapper`]; the others keep their
    /// own mechanisms (native hooks / plugin / bundled extension). This guards
    /// against a stray registry edit wiring another agent to the wrapper.
    #[test]
    fn only_codex_uses_wrapper_strategy() {
        assert_eq!(
            spec(&AgentType::Codex).strategy,
            Some(IntegrationStrategy::Wrapper)
        );
        for spec in ALL {
            if spec.agent_type == AgentType::Codex {
                continue;
            }
            assert_ne!(
                spec.strategy,
                Some(IntegrationStrategy::Wrapper),
                "only Codex should use the Wrapper strategy"
            );
        }
    }

    /// `ALL` holds exactly the shipped, detectable agents (the neutral
    /// placeholder is excluded), and each entry round-trips through detection.
    #[test]
    fn all_holds_shipped_agents_and_round_trips() {
        let types: Vec<&AgentType> = ALL.iter().map(|spec| &spec.agent_type).collect();
        assert_eq!(
            types,
            vec![
                &AgentType::ClaudeCode,
                &AgentType::OpenCode,
                &AgentType::Pi,
                &AgentType::Codex
            ]
        );
        assert!(!ALL.iter().any(|spec| spec.agent_type == AgentType::None));

        // Every shipped agent is detectable from at least one basename, and
        // that basename resolves back to the same type.
        for spec in ALL {
            let basename = spec
                .detect_basenames
                .first()
                .expect("a shipped agent must have a detection basename");
            assert_eq!(
                detect_from_basename(basename),
                Some(spec.agent_type.clone())
            );
        }
    }

    /// PRD #20 finding #15: strategy dispatch resolves from the SPEC's own
    /// handler, not a hardcoded incumbent keyed by the strategy enum. Each
    /// shipped agent carries exactly the handlers its strategy needs, a Wrapper
    /// agent and the neutral placeholder carry none, and two agents that share a
    /// hook-install shape resolve to DIFFERENT handlers — so a future agent
    /// reusing an existing strategy runs its own implementation, never another
    /// agent's module.
    #[test]
    fn strategy_handlers_resolve_from_spec_not_incumbent() {
        assert!(spec(&AgentType::ClaudeCode).hook_install.is_some());
        assert!(spec(&AgentType::ClaudeCode).hook_uninstall.is_some());
        assert!(spec(&AgentType::ClaudeCode).materialize.is_none());

        assert!(spec(&AgentType::OpenCode).hook_install.is_some());
        assert!(spec(&AgentType::OpenCode).hook_uninstall.is_some());
        assert!(spec(&AgentType::OpenCode).materialize.is_none());

        assert!(spec(&AgentType::Pi).materialize.is_some());
        assert!(spec(&AgentType::Pi).hook_install.is_none());

        // A Wrapper agent (Codex) and the neutral placeholder carry no
        // hook-install / materialize handlers.
        assert!(spec(&AgentType::Codex).hook_install.is_none());
        assert!(spec(&AgentType::Codex).materialize.is_none());
        assert!(spec(&AgentType::None).hook_install.is_none());
        assert!(spec(&AgentType::None).materialize.is_none());

        // Claude and OpenCode install through DIFFERENT handlers — proof the
        // handler is sourced per-spec, not from one per-strategy incumbent.
        let claude = spec(&AgentType::ClaudeCode)
            .hook_install
            .expect("Claude has an installer");
        let opencode = spec(&AgentType::OpenCode)
            .hook_install
            .expect("OpenCode has an installer");
        assert!(
            !std::ptr::fn_addr_eq(claude, opencode),
            "each agent must resolve to its OWN installer, not a shared incumbent"
        );
    }

    /// The badge colour field is populated for every entry (single source of
    /// truth for the later badge-rendering milestone), and the neutral
    /// placeholder gets a deliberately neutral colour distinct from the real
    /// agents'.
    #[test]
    fn badge_colours_present_and_neutral_for_none() {
        assert_eq!(spec(&AgentType::None).badge_color, Color::DarkGray);
        for spec in ALL {
            assert_ne!(
                spec.badge_color,
                Color::DarkGray,
                "a shipped agent's badge should not reuse the neutral placeholder colour"
            );
        }
    }
}
