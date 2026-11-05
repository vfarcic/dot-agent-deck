//! PRD #1666 part 3: the per-agent switch that turns the doorbell off.
//!
//! The doorbell works from one set of facts per agent
//! ([`crate::doorbell_activation::AGENT_FACTS`]), so a new agent release is
//! typed into before anyone has measured it. A user who meets a release that
//! breaks it turns it off for that agent until a deck release fixes the
//! facts: `doorbell.claude` / `doorbell.codex` in the deck's `config.toml`
//! ([`crate::config::DoorbellConfig`]), or `DOT_AGENT_DECK_DOORBELL_CLAUDE` /
//! `DOT_AGENT_DECK_DOORBELL_CODEX`, which win over the file the way
//! `DOT_AGENT_DECK_EXPERIMENTAL` wins over its own.
//!
//! The daemon reads it once per agent it starts ([`at_spawn`]): installation
//! of the input-box check is decided then
//! (`crate::composer_probe::install::at_spawn`), and the generation keeps the
//! value for its life, so a change applies to agents started after it. Off,
//! the generation's verdict is `turned_off` (`NoGo::TurnedOff`), whatever its
//! screen shows; nothing else about delivery changes.

use crate::event::AgentType;

/// The variable that overrides `doorbell.claude`.
pub const CLAUDE_ENV: &str = "DOT_AGENT_DECK_DOORBELL_CLAUDE";
/// The variable that overrides `doorbell.codex`.
pub const CODEX_ENV: &str = "DOT_AGENT_DECK_DOORBELL_CODEX";

/// Whether the doorbell is on, per agent that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorbellSwitch {
    pub claude: bool,
    pub codex: bool,
}

impl Default for DoorbellSwitch {
    /// Both on.
    fn default() -> Self {
        Self {
            claude: true,
            codex: true,
        }
    }
}

impl DoorbellSwitch {
    /// Whether the doorbell may run for `agent`. An agent with no switch
    /// (one the deck never types into, or none known yet) is not turned off
    /// by it: what holds those is the activation table.
    pub fn allows(&self, agent: &AgentType) -> bool {
        match agent {
            AgentType::ClaudeCode => self.claude,
            AgentType::Codex => self.codex,
            _ => true,
        }
    }

    /// The switch from `config`, each value overridden by its variable when
    /// `env` reads one that names a state: `1`, `true` or `on` turn it on,
    /// `0`, `false` or `off` turn it off (any case, surrounding space
    /// ignored); anything else leaves the file's value.
    pub fn resolve(
        config: &crate::config::DoorbellConfig,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let read = |name: &str, file: bool| env(name).and_then(|v| parse_state(&v)).unwrap_or(file);
        Self {
            claude: read(CLAUDE_ENV, config.claude),
            codex: read(CODEX_ENV, config.codex),
        }
    }
}

/// `1`/`true`/`on` → on, `0`/`false`/`off` → off, anything else → `None`.
pub fn parse_state(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    }
}

/// The switch for an agent the daemon is starting now: the deck's
/// `config.toml`, read with the daemon's bounded reader, under the
/// environment's overrides.
#[cfg(not(test))]
pub fn at_spawn() -> DoorbellSwitch {
    let config = crate::config::DashboardConfig::load_bounded();
    DoorbellSwitch::resolve(&config.doorbell, |name| std::env::var(name).ok())
}

/// Unit tests read neither the developer's `config.toml` nor their
/// environment: a test that needs the switch off sets it on the generation.
#[cfg(test)]
pub fn at_spawn() -> DoorbellSwitch {
    DoorbellSwitch::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DoorbellConfig;
    use spec::spec;

    /// Scenario: Resolve the doorbell switch from the config's `[doorbell]` table and the two environment variables. With neither, both agents are on; the file turns one off; a variable naming a state (`0`, `false`, `off`, `1`, `true`, `on`, any case) wins over the file in either direction, and any other value leaves the file's; agents without a switch are never turned off by it.
    #[spec("inbox/doorbell/059")]
    #[test]
    fn doorbell_059_the_switch_resolves_from_the_file_and_the_environment() {
        let on = DoorbellConfig::default();
        let none = |_: &str| None;
        assert_eq!(
            DoorbellSwitch::resolve(&on, none),
            DoorbellSwitch::default()
        );
        let claude_off = DoorbellConfig {
            claude: false,
            codex: true,
        };
        let s = DoorbellSwitch::resolve(&claude_off, none);
        assert!(!s.allows(&AgentType::ClaudeCode));
        assert!(s.allows(&AgentType::Codex));
        for (value, expect) in [
            ("0", Some(false)),
            ("false", Some(false)),
            (" OFF ", Some(false)),
            ("1", Some(true)),
            ("True", Some(true)),
            ("on", Some(true)),
            ("", None),
            ("no", None),
            ("2", None),
        ] {
            assert_eq!(parse_state(value), expect, "{value:?}");
            for file in [true, false] {
                let config = DoorbellConfig {
                    claude: file,
                    codex: file,
                };
                let s = DoorbellSwitch::resolve(&config, |name| {
                    (name == CODEX_ENV).then(|| value.to_string())
                });
                assert_eq!(s.codex, expect.unwrap_or(file), "{value:?} over {file}");
                assert_eq!(s.claude, file, "the Codex variable leaves Claude alone");
            }
        }
        let all_off = DoorbellSwitch {
            claude: false,
            codex: false,
        };
        for agent in [
            AgentType::None,
            AgentType::OpenCode,
            AgentType::Pi,
            AgentType::Devin,
        ] {
            assert!(all_off.allows(&agent), "{agent:?}");
        }
    }
}
