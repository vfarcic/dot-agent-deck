//! Default-shell and command-wrap policy (PRD #42 M1, lifted from
//! `agent_pty.rs`).
//!
//! A multi-word agent command is run through the platform shell rather than
//! exec'd directly. Unix uses `$SHELL -c …` (falling back to `/bin/sh`);
//! Windows uses `%COMSPEC% /C …` (falling back to `cmd.exe`).

/// Whether a `command` must be run through `<shell> <flag>` rather than exec'd
/// directly. A command with whitespace is a shell command line (pipes, `;`,
/// redirections, multiple words); a single bare word is exec'd directly.
///
/// Platform-independent: the predicate is the same on Unix and Windows; only
/// the *shell* used for the wrap (see [`default_shell`] / [`shell_command_flag`])
/// differs.
pub fn command_needs_shell_wrap(command: &str) -> bool {
    command.contains(char::is_whitespace)
}

/// Resolve the shell used for the `-c`/`/C` wrap of a multi-word command and
/// for the no-command fallback.
///
/// A caller may pin the shell with `shell_override` (PRD #127 M2.1: the
/// scheduler injects `SHELL` to run an explicit multi-word command under a
/// deterministic `/bin/sh -c` while reserving the daemon's own `$SHELL` for the
/// omitted-command fallback). When unset:
/// - Unix: the process `$SHELL`, then `/bin/sh`.
/// - Windows: the process `%COMSPEC%`, then `cmd.exe`.
pub fn default_shell(shell_override: Option<&str>) -> String {
    if let Some(s) = shell_override {
        return s.to_string();
    }
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
}

/// Flag passed to [`default_shell`] to run a command string: `-c` on Unix,
/// `/C` on Windows (`cmd.exe`).
pub fn shell_command_flag() -> &'static str {
    #[cfg(unix)]
    {
        "-c"
    }
    #[cfg(windows)]
    {
        "/C"
    }
}
