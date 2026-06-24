//! Re-export shim for the shared [`keybindings`] crate (PRD #176).
//!
//! The keybinding model — the [`Action`](::keybindings::Action) set, the
//! notation parser, [`KeybindingConfig`](::keybindings::KeybindingConfig), and
//! the TOML/defaults loader (originally PRD #40's `src/keybindings.rs`) — moved
//! out of this binary into the `keybindings` workspace crate so the TUI and the
//! desktop GUI core resolve IDENTICAL bindings from one source. This module
//! re-exports it unchanged, so existing `crate::keybindings::…` /
//! `dot_agent_deck::keybindings::…` paths keep working with no behavior change.
pub use ::keybindings::*;
