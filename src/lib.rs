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
pub mod embedded_pane;
pub mod error;
pub mod event;
pub mod features;
pub mod hook;
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
pub mod project_config;
pub mod prompt_delivery;
pub mod remote;
pub mod remote_doctor;
pub mod schedule_cli;
pub mod scheduler;
pub mod spawn;
pub mod state;
pub mod tab;
pub mod tab_layout;
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
#[cfg(test)]
mod test_isolation;
pub mod ui;
// Issue #670: the one implementation of the control-character / Unicode-bidi
// filter applied to producer-supplied strings before they reach a terminal.
pub mod untrusted_text;
pub mod version;
pub mod watch;
pub mod worktree_owner;
pub mod worktree_reclaim;
pub mod wrap;
