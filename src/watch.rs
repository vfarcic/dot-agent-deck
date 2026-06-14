use std::io::Write;
use std::process::{Command, Stdio};

/// Run a command repeatedly at a fixed interval, clearing the screen between runs.
///
/// Used internally by mode manager for persistent panes (`watch = true`) and
/// reactive watch rules (`watch = true` in `.dot-agent-deck.toml`).
pub fn run_watch(interval_secs: u64, command: &str) -> ! {
    let mut first = true;
    loop {
        if !first {
            std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        }
        first = false;

        // Capture command output, then clear and print in one shot
        // to avoid auto-scroll issues with direct PTY output.
        //
        // PRD #42 M1: route the shell-wrap through the `platform::shell` flag
        // seam so the invocation is Windows-correct (`cmd.exe /C …`) instead of
        // the previously-hardcoded `sh`. Unix behavior is preserved exactly:
        // the watch command is a fixed POSIX command line, so Unix keeps the
        // deterministic `sh -c …` rather than switching to `$SHELL`.
        #[cfg(unix)]
        let shell = "sh";
        #[cfg(windows)]
        let shell = crate::platform::shell::default_shell(None);
        let output = Command::new(shell)
            .arg(crate::platform::shell::shell_command_flag())
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        // Clear scrollback + screen + cursor home, then print captured output
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x1b[3J\x1b[2J\x1b[H");

        match output {
            Ok(out) => {
                let _ = stdout.write_all(&out.stdout);
                if !out.stderr.is_empty() {
                    let _ = stdout.write_all(&out.stderr);
                }
            }
            Err(e) => {
                let _ = writeln!(stdout, "[error: {e}]");
            }
        }
        let _ = stdout.flush();
    }
}
