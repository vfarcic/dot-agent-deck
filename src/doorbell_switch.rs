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
//! value for its life, so a change applies to agents started after it. A
//! `config.toml` that is there but cannot be used (malformed, a wrong-typed
//! value, too large, unreadable, a link whose target is missing) holds the doorbell off for both agents rather
//! than falling back to the default of on: the file may be the one that turned
//! an agent off, and the variables still win over it. Off,
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

    /// The switch from a read of the deck's `config.toml`
    /// ([`crate::config::DashboardConfig::load_bounded_status`]) under `env`'s
    /// overrides ([`Self::resolve`]). A file that is there but could not be
    /// used holds the doorbell off for both agents: its defaults are on, and
    /// the file may be the one that turned an agent off.
    pub fn from_load(
        load: &crate::config::BoundedLoad,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let held = crate::config::DoorbellConfig {
            claude: false,
            codex: false,
        };
        let file = match load.file {
            crate::config::ConfigFile::Failed => &held,
            crate::config::ConfigFile::Absent | crate::config::ConfigFile::Read => {
                &load.config.doorbell
            }
        };
        Self::resolve(file, env)
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
/// environment's overrides. A file that is there but cannot be used holds both
/// agents off ([`DoorbellSwitch::from_load`]); its warning is in the daemon's
/// log.
#[cfg(not(test))]
pub fn at_spawn() -> DoorbellSwitch {
    let load = crate::config::DashboardConfig::load_bounded_status();
    DoorbellSwitch::from_load(&load, |name| std::env::var(name).ok())
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

    /// Scenario: Read the doorbell switch from a `config.toml` that is absent, valid, malformed, has a wrong-typed value, or is over the size limit. An absent file leaves both agents on and a valid one says what it says; a file that is there but can't be used holds the doorbell off for both agents rather than falling back to on, and a variable still wins over that.
    #[spec("inbox/doorbell/060")]
    #[test]
    fn doorbell_060_a_config_that_cannot_be_used_holds_the_doorbell_off() {
        use crate::config::{ConfigFile, DashboardConfig};
        let dir = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        let off = DoorbellSwitch {
            claude: false,
            codex: false,
        };

        let absent = DashboardConfig::load_bounded_status_from(&dir.path().join("absent.toml"));
        assert_eq!(absent.file, ConfigFile::Absent);
        assert_eq!(
            DoorbellSwitch::from_load(&absent, none),
            DoorbellSwitch::default(),
            "no file: the defaults, both on"
        );

        let valid = dir.path().join("valid.toml");
        std::fs::write(&valid, "[doorbell]\ncodex = false\n").unwrap();
        let load = DashboardConfig::load_bounded_status_from(&valid);
        assert_eq!(load.file, ConfigFile::Read);
        assert_eq!(
            DoorbellSwitch::from_load(&load, none),
            DoorbellSwitch {
                claude: true,
                codex: false
            }
        );

        let oversize = "#".repeat(crate::config::MAX_DASHBOARD_CONFIG_BYTES as usize + 1);
        for (name, contents) in [
            ("malformed", "[doorbell\nclaude = false\n".to_string()),
            ("wrong-typed", "[doorbell]\nclaude = \"no\"\n".to_string()),
            ("wrong-typed-elsewhere", "default_command = 7\n".to_string()),
            ("oversize", oversize),
        ] {
            let path = dir.path().join(format!("{name}.toml"));
            std::fs::write(&path, contents).unwrap();
            let load = DashboardConfig::load_bounded_status_from(&path);
            assert_eq!(load.file, ConfigFile::Failed, "{name}");
            assert_eq!(DoorbellSwitch::from_load(&load, none), off, "{name}");
            let codex_on =
                DoorbellSwitch::from_load(&load, |var| (var == CODEX_ENV).then(|| "1".to_string()));
            assert_eq!(
                codex_on,
                DoorbellSwitch {
                    claude: false,
                    codex: true
                },
                "{name}: the variable still wins"
            );
        }
    }

    /// Scenario: Read the doorbell switch from a `config.toml` that is a symlink: to a missing target, to a readable file, to a folder, and in a loop, next to a path with nothing at it. Only the path with nothing at it reads as absent and leaves both agents on; a link to a readable file says what the file says; a link whose target is missing, a folder or a loop is a config that is there but can't be used, and holds the doorbell off for both agents, with a variable still winning over it.
    #[spec("inbox/doorbell/061")]
    #[cfg(unix)]
    #[test]
    fn doorbell_061_a_config_link_whose_target_is_missing_holds_the_doorbell_off() {
        use crate::config::{ConfigFile, DashboardConfig};
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let none = |_: &str| None;
        let off = DoorbellSwitch {
            claude: false,
            codex: false,
        };

        let nothing = dir.path().join("nothing.toml");
        let load = DashboardConfig::load_bounded_status_from(&nothing);
        assert_eq!(load.file, ConfigFile::Absent, "no file and no link");
        assert!(load.warnings.is_empty(), "{:?}", load.warnings);
        assert_eq!(
            DoorbellSwitch::from_load(&load, none),
            DoorbellSwitch::default()
        );

        let file = dir.path().join("dotfiles-deck.toml");
        std::fs::write(&file, "[doorbell]\nclaude = false\n").unwrap();
        let to_file = dir.path().join("to-file.toml");
        symlink(&file, &to_file).unwrap();
        let load = DashboardConfig::load_bounded_status_from(&to_file);
        assert_eq!(load.file, ConfigFile::Read, "a link to a readable file");
        assert_eq!(
            DoorbellSwitch::from_load(&load, none),
            DoorbellSwitch {
                claude: false,
                codex: true
            }
        );

        let dangling = dir.path().join("dangling.toml");
        symlink(dir.path().join("unmounted/deck.toml"), &dangling).unwrap();
        let dangling_relative = dir.path().join("dangling-relative.toml");
        symlink("moved-away.toml", &dangling_relative).unwrap();
        let chained = dir.path().join("chained.toml");
        symlink(&dangling, &chained).unwrap();
        let to_dir = dir.path().join("to-dir.toml");
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        symlink(&folder, &to_dir).unwrap();
        let looped = dir.path().join("loop-a.toml");
        let loop_b = dir.path().join("loop-b.toml");
        symlink(&loop_b, &looped).unwrap();
        symlink(&looped, &loop_b).unwrap();

        for path in [dangling, dangling_relative, chained, to_dir, looped] {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let load = DashboardConfig::load_bounded_status_from(&path);
            assert_eq!(load.file, ConfigFile::Failed, "{name}");
            assert_eq!(load.warnings.len(), 1, "{name}: {:?}", load.warnings);
            assert_eq!(DoorbellSwitch::from_load(&load, none), off, "{name}");
            assert_eq!(
                DoorbellSwitch::from_load(&load, |var| (var == CLAUDE_ENV).then(|| "on".into())),
                DoorbellSwitch {
                    claude: true,
                    codex: false
                },
                "{name}: the variable still wins"
            );
        }
    }
}
