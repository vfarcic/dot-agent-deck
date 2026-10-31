// PRD #1666 part 3 (Q59): the `e2e` feature compiles in test seams, many of
// them steered by environment variables (`host_metrics::e2e_fixed_sample` and
// `daemon_protocol::advertised_protocol_version` are two), and none may ship.
// A release-profile build turns `debug_assertions` off, so this refuses
// `--release --features e2e`. `not(debug_assertions)` is the predicate, not
// the profile: a dev build with debug assertions switched off is refused too,
// and a release build with them switched on is not, so a shipping recipe must
// still leave `e2e` out.
#[cfg(all(feature = "e2e", not(debug_assertions)))]
compile_error!(
    "the `e2e` feature is a test seam and must never ship in a release build: \
     build `--features e2e` only with debug assertions on (the dev and test \
     profiles), and build releases without it"
);

// Shared by the per-agent hook-config adapters (`codex_hooks_manage`,
// `devin_hooks_manage`); nothing outside the crate calls it.
mod agent_hook_config;
pub mod agent_pty;
pub mod agent_registry;
pub mod attach_recovery;
pub mod authoring_seeds;
pub mod bounded_read;
pub mod build_id;
pub mod build_version_handshake;
// PRD #1589: the daemon side of `close` — resolve, authorize, refuse or close.
pub mod close_agents;
// PRD #1589: the `close` command's request building and report rendering.
pub mod close_cli;
pub mod codex_hooks_manage;
pub mod codex_rollout_tail;
pub mod config;
pub mod config_gen;
pub mod config_validation;
// PRD #1487: refuses agent-config writes outside an owned root in a test process.
pub mod config_write_guard;
pub mod connect;
pub mod daemon;
pub mod daemon_attach;
pub mod daemon_client;
pub mod daemon_protocol;
pub mod daemon_restart;
pub mod daemon_service;
// Issue #1490: why a deck is not connected, and starting its daemon.
pub mod daemon_start;
pub mod daemon_status;
pub mod daemon_stop;
pub mod daemon_upgrade;
pub mod deck_list;
// PRD #1666: the deck's persistent id, carried in inbox addresses and message ids.
pub mod deck_identity;
pub mod devin_hooks_manage;
// PRD #1223 M1: the daemon's one-level, bounded directory listing that backs
// the desktop's new-agent directory step.
pub mod directory_listing;
pub mod dispatch;
pub mod dispatch_return;
// PRD #1666 S4: the fixed one-line doorbell for an agent with no armed inbox component.
pub mod doorbell;
// PRD #1666 part 3 round 5-wake: which agent releases the doorbell may type into.
pub mod doorbell_activation;
pub mod doorbell_generation;
// PRD #1666 part 3 round 5-wake: an agent's screen read losslessly, and the measured recognisers.
pub mod doorbell_screen;
// PRD #1666 part 3 round 5-wake: the doorbell transaction as a pure model, inert until activation.
pub mod doorbell_txn;
// PRD #1589: the record of dispatched units the `close` verb is authorized from.
pub mod dispatched_units;
// PRD #1666 part 3 round 5-wake (R6-4): an agent's input box, followed from its PTY input.
pub mod composer_model;
// PRD #1666 part 3 (Q35): checking an agent's input box through its own Ctrl+G.
pub mod composer_probe;
// Issue #544: the per-pane unsent-draft bit the doorbell reads before it writes.
pub mod draft_deferral;
pub mod embedded_pane;
// PRD #1419 Decision 4: the user docs embedded at build time for `docs [topic]`.
pub mod embedded_docs;
// Issue #1121: the endpoint I/O that `platform::paths`' pure resolvers must not
// do — creating the owner-only fallback directory, and the connect side's
// read-only probe of the pre-#1121 endpoint spelling.
pub mod endpoint_resolve;
// Issue #1516: an in-process stand-in for a few environment knobs, so a test can
// change one while the deck's own tasks are reading it. Production never sets an
// override; the environment variables stay the user-facing knobs.
#[doc(hidden)]
pub mod env_override;
pub mod error;
pub mod event;
pub mod event_subscriber;
// PRD #1666 part 3 round 5-wake: whether a parsed frame was drawn for the PTY's current width.
pub mod evidence_state;
pub mod features;
pub mod guarded;
// PRD #1105 M11: when the TUI claims focus on its daemon (terminal focus-in and
// throttled input).
pub mod focus_report;
// PRD #1258: measurements of the host the process runs on. Its load-per-CPU is
// the one implementation the lib tests' `test_budget` and the test harness both
// call (M2).
pub mod host_metrics;
// Issue #1181: the ambient git location environment, and the one place this
// crate switches it off. Every `git` the crate spawns is built here.
pub(crate) mod git_env;
pub mod hook;
pub mod hook_binary;
mod hook_event_order;
pub mod hook_provenance;
pub mod hooks_manage;
pub mod hooks_refresh;
pub mod hyperlink;
// PRD #1666: the per-agent inbox every deck message is posted to (pure data).
pub mod inbox;
// PRD #1666 part 3 round 5-wake: human operations on an agent's input, arrival to PTY.
pub mod inbox_attempts;
pub mod inbox_cli;
pub mod inbox_display;
pub mod inbox_hub;
pub mod inbox_oneshot;
pub mod inbox_prompt;
pub mod inbox_pubsub;
pub mod inbox_receipt;
pub mod inbox_receiver;
pub mod inbox_spills;
pub mod inbox_watcher;
pub mod ingress_ledger;
pub mod init;
pub mod injected_prompt;
// PRD #1666 part 3 round 5-wake: a wrapper's samples of its agent's line discipline.
pub mod input_mode;
pub mod issue_dispatch;
pub mod issue_dispatch_run;
pub mod jsonc_edit;
pub mod keybindings;
// Issue #1540: the deck's last New-agent-form command, owned by the daemon.
pub mod last_command;
pub mod lifetime_tag;
pub mod lock_rank;
pub mod logging;
pub mod login_shell;
// PRD #1223 M2: what the daemon reports about itself to a new-agent form.
pub mod new_agent_options;
pub mod opencode_manage;
// Issue #1947: finding and messaging an orchestration's orchestrator.
pub mod orchestrator_address;
pub mod orchestrator_context;
pub mod orchestrator_ext;
pub mod osc52;
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
// Issue #1884: the bounded macOS retry of PTY allocation.
pub mod pty_open_retry;
// PRD #1401: the pull request an agent's work produced, on the session wire.
pub mod pull_request;
pub mod pull_request_info;
pub mod question;
pub mod quota_block;
pub mod quota_signals;
// PRD #1666 part 3 round 5-wake: an agent's release, read off its start-up header.
pub mod release_scanner;
pub mod remote;
pub mod remote_daemon;
pub mod remote_doctor;
pub mod remote_tunnel;
pub mod repo_identity;
pub mod report_recovery;
pub mod retained_store;
pub mod roles_cli;
pub mod schedule_cli;
// Issue #1635: upgrading this machine's CLI and desktop app in place.
pub mod scheduler;
pub mod self_upgrade;
pub mod spawn;
pub mod state;
pub mod submitted_prompt;
// PRD #1401: the TUI's `o` key, which hands a wire URL to the system browser.
pub mod system_browser;
pub mod tab;
pub mod tab_layout;
pub mod task_completion;
pub mod work_done_text;
// PRD #1741: agent-run telemetry over OTLP, off unless an endpoint is set.
pub mod telemetry;
pub mod terminal_hangup;
// Issue #1537: the DEC private-mode parser shared by the TUI's mouse-mode scan
// and the daemon's output ring, which restores those modes on replay.
pub(crate) mod terminal_modes;
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
// Issue #1637: test-only. A Unix-style fixture path is not absolute on Windows,
// so tests of absolute-vs-relative classification spell their paths through it.
#[cfg(test)]
mod test_paths;
// Issue #1132: test-only, and shared for the same reason as the two above —
// unit tests in `spawn.rs`, `ui.rs` and `state.rs` all drive the same `/bin/cat`
// PTY byte target, and the content-keyed waits that make their assertions
// deterministic were private to one of them.
#[cfg(test)]
mod test_pty_wait;
pub mod turn_reply;
pub mod ui;
// Issue #670: the one implementation of the control-character / Unicode-bidi
// filter applied to producer-supplied strings before they reach a terminal.
pub mod untrusted_text;
pub mod upgrade_dialog;
pub mod version;
// vt100 0.18 sizes grids in `NonZeroU16`; the deck's `u16` geometry crosses here.
pub mod vt_grid;
pub mod vt_work;
pub mod watch;
pub mod worktree_owner;
pub mod worktree_reclaim;
pub mod wrap;
