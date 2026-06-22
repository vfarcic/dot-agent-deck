//! Render the config-generation prompt for a project directory — byte-for-byte
//! what dot-agent-deck sends to the config-gen agent (see
//! `crate::config_gen::config_gen_prompt`, invoked from
//! `send_config_gen_prompt` in `src/ui.rs`).
//!
//! Used by the PRD #116 baseline-regeneration procedure to capture the exact
//! post-render prompt for auditability. The directory is interpolated into the
//! prompt's `{dir}` placeholder and the embedded role library into `{roles}`.
//!
//! Usage:
//!   cargo run --quiet --example render_config_gen_prompt -- <project_dir>
//!
//! Prints the rendered prompt to stdout with no trailing newline added.

use dot_agent_deck::config_gen::config_gen_prompt;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    print!("{}", config_gen_prompt(&dir));
}
