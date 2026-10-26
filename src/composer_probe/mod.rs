//! PRD #1666 part 3 (Q35): the composer probe — how the deck checks an
//! agent's input box through the agent's own external-editor key (Ctrl+G)
//! instead of following its keystrokes, and holds what a user types while a
//! check runs.
//!
//! Built a commit at a time (Q35-core's implementation plan). Today it holds
//! the framing tokenizer ([`framing`]), the output hook with its
//! bracketed-paste tracker ([`paste_mode`]), the rule for trusting the
//! file an agent exports ([`path_rule`]) and the classifier of whose editor
//! invocation it is ([`classifier`]). The daemon feeds the hook every byte an
//! agent writes; nothing in it reads what the hook tracks yet, or calls the
//! path rule or the classifier.

pub mod classifier;
pub mod framing;
pub mod paste_mode;
pub mod path_rule;
