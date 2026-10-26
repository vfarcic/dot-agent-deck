//! PRD #1666 part 3 (Q35): the composer probe — how the deck checks an
//! agent's input box through the agent's own external-editor key (Ctrl+G)
//! instead of following its keystrokes, and holds what a user types while a
//! check runs.
//!
//! Built a commit at a time (Q35-core's implementation plan). Today it holds
//! the framing tokenizer ([`framing`]) and the output hook with its
//! bracketed-paste tracker ([`paste_mode`]). The daemon feeds the hook every
//! byte an agent writes; nothing reads what it tracks yet.

pub mod framing;
pub mod paste_mode;
