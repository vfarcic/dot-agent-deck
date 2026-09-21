// Shared by the per-agent hook-config adapters (`codex_hooks_manage`,
// `devin_hooks_manage`); nothing outside the crate calls it.
mod agent_hook_config;
pub mod agent_pty;
pub mod agent_registry;
pub mod bounded_read;
pub mod build_id;
pub mod build_version_handshake;
pub mod codex_hooks_manage;
pub mod config;
pub mod config_gen;
pub mod config_validation;
pub mod connect;
pub mod daemon;
pub mod daemon_attach;
pub mod daemon_client;
pub mod daemon_protocol;
pub mod daemon_status;
pub mod daemon_stop;
pub mod devin_hooks_manage;
pub mod dispatch;
pub mod dispatch_return;
pub mod embedded_pane;
pub mod error;
pub mod event;
pub mod features;
// PRD #1105 M11: when the TUI claims focus on its daemon (terminal focus-in and
// throttled input).
pub mod focus_report;
// Issue #1181: the ambient git location environment, and the one place this
// crate switches it off. Every `git` the crate spawns is built here.
pub(crate) mod git_env;
pub mod hook;
pub mod hook_provenance;
pub mod hooks_manage;
pub mod hyperlink;
pub mod init;
pub mod issue_dispatch;
pub mod issue_dispatch_run;
pub mod keybindings;
pub mod lifetime_tag;
pub mod logging;
pub mod login_shell;
pub mod mode_manager;
pub mod opencode_manage;
pub mod orchestrator_context;
pub mod orchestrator_ext;
pub mod palette;
pub mod pane;
pub mod pane_input;
pub mod pane_screen_text;
pub mod platform;
// PRD #819 M4: the short-lived preparation token that bridges the gap between
// the launch verb and the `StartAgent` sequence that actually spawns.
pub mod prep_token;
pub mod project_config;
// PRD #819 M3: the bounded, symlink-safe project reader and the daemon-side
// enumeration built on it. Separate from `project_config` because the loader
// there is for files this process wrote, and this one is for a path a caller
// selected over the attach socket.
pub mod project_resolve;
pub mod prompt_delivery;
pub mod remote;
pub mod remote_doctor;
pub mod remote_tunnel;
pub mod repo_identity;
pub mod schedule_cli;
pub mod scheduler;
pub mod spawn;
pub mod state;
pub mod tab;
pub mod tab_layout;
pub mod terminal_hangup;
pub mod terminal_widget;
// Issue #322: test-only, and never part of the shipped library. Unit tests in
// this crate do not link `tests/common/`, so before this they allocated scratch
// space in the OS temp dir — the RAM-backed `/tmp` the issue is about.
#[cfg(test)]
mod test_temp;
// Issue #666 follow-up: test-only, for the same reason as `test_temp` above.
// Unit tests do not link `tests/common/`, so `init_test_env`'s scrub of the deck
// endpoint variables never ran for them and a fixture that spawned an emitter
// posted hook events into the developer's live dashboard.
// Issue #709's load-scaled wait ceilings, for the lib target's own unit tests.
// `tests/common/mod.rs` has the same helper and the full rationale, but this
// target does not link that file — the wall `test_isolation` documents.
#[cfg(test)]
mod test_budget;
#[cfg(test)]
mod test_isolation;
// Issue #1132: test-only, and shared for the same reason as the two above —
// unit tests in `spawn.rs`, `ui.rs` and `state.rs` all drive the same `/bin/cat`
// PTY byte target, and the content-keyed waits that make their assertions
// deterministic were private to one of them.
#[cfg(test)]
mod test_pty_wait;
pub mod ui;
// Issue #670: the one implementation of the control-character / Unicode-bidi
// filter applied to producer-supplied strings before they reach a terminal.
pub mod untrusted_text;
pub mod version;
pub mod watch;
pub mod worktree_owner;
pub mod worktree_reclaim;
pub mod wrap;
