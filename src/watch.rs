use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Clear scrollback + screen, then home the cursor.
const CLEAR: &[u8] = b"\x1b[3J\x1b[2J\x1b[H";

/// Run a command repeatedly at a fixed interval, clearing the screen between runs.
///
/// The body of the standalone `dot-agent-deck watch` subcommand.
pub fn run_watch(interval_secs: u64, command: &str) -> ! {
    let sink = Mutex::new(std::io::stdout());
    let mut first = true;
    loop {
        if !first {
            std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        }
        first = false;

        run_once(command, &sink);
    }
}

/// Run `command` once, streaming its output to `sink` as the bytes arrive.
///
/// Issue #367: this used to `output()` the child — buffering everything and
/// printing it only after the process exited. Every fast command looked fine,
/// but a command that does not exit (`tail -f`, `kubectl logs -f`, a dev
/// server) produced a **permanently blank pane**: its bytes sat in the pipe
/// forever, with no error and no hint.
///
/// The screen is therefore cleared lazily, on the **first byte of output**,
/// rather than after the command exits. That keeps the property the old
/// buffer-then-print was written for — no visible blank gap while a fast
/// command runs, so no flicker between refreshes — while letting a
/// long-running command paint as it produces output. A command that produces
/// nothing at all still clears on exit, so a previous pass's output never
/// lingers as if it were current.
///
/// stdout and stderr are pumped concurrently and appear in arrival order
/// (previously all of stdout, then all of stderr).
fn run_once<W: Write + Send>(command: &str, sink: &Mutex<W>) {
    // PRD #42 M1 / #163 M1: route both the shell and the `-c`/`/C` flag
    // through the `platform::shell` seam so the invocation is
    // Windows-correct (`cmd.exe /C …`) instead of the previously-hardcoded
    // `sh`. Unix behavior is preserved exactly: the watch command is a fixed
    // POSIX command line, so Unix keeps the deterministic `sh -c …` rather
    // than switching to `$SHELL`.
    let shell = crate::platform::shell::fixed_command_shell("sh");
    let spawned = Command::new(shell)
        .arg(crate::platform::shell::shell_command_flag())
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            write_locked(sink, |out| {
                let _ = out.write_all(CLEAR);
                let _ = writeln!(out, "[error: {e}]");
            });
            return;
        }
    };

    let cleared = AtomicBool::new(false);
    let pipes = [
        child.stdout.take().map(PipeReader::Stdout),
        child.stderr.take().map(PipeReader::Stderr),
    ];

    std::thread::scope(|scope| {
        for pipe in pipes.into_iter().flatten() {
            scope.spawn(|| pump(pipe, sink, &cleared));
        }
    });

    let _ = child.wait();

    if !cleared.load(Ordering::SeqCst) {
        // The command produced nothing this pass. Clear anyway so the
        // previous pass's output does not linger as if it were current —
        // the behavior the unconditional post-exit clear used to give.
        write_locked(sink, |out| {
            let _ = out.write_all(CLEAR);
        });
    }
}

/// The two child pipes, unified so one `pump` handles both. `ChildStdout`
/// and `ChildStderr` are distinct types with no shared trait object here.
enum PipeReader {
    Stdout(std::process::ChildStdout),
    Stderr(std::process::ChildStderr),
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PipeReader::Stdout(r) => r.read(buf),
            PipeReader::Stderr(r) => r.read(buf),
        }
    }
}

/// Copy one pipe into `sink` until EOF, clearing the screen ahead of the very
/// first byte written by *either* pipe.
fn pump<R: Read, W: Write>(mut reader: R, sink: &Mutex<W>, cleared: &AtomicBool) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => write_locked(sink, |out| {
                // `swap` under the sink lock: the clear and the bytes it
                // precedes stay one atomic write even with both pipes live.
                if !cleared.swap(true, Ordering::SeqCst) {
                    let _ = out.write_all(CLEAR);
                }
                let _ = out.write_all(&buf[..n]);
            }),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// Run `f` against the locked sink and flush. A panicking writer elsewhere
/// must not silence this pane, so a poisoned lock is recovered rather than
/// propagated.
fn write_locked<W: Write>(sink: &Mutex<W>, f: impl FnOnce(&mut W)) {
    let mut out = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut out);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn captured(sink: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&sink.lock().unwrap()).into_owned()
    }

    /// Issue #367's core regression: output must reach the pane while the
    /// command is still running, not only once it exits.
    #[test]
    fn streams_output_before_a_long_running_command_exits() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&sink);
        // Prints immediately, then stays alive far longer than this test.
        std::thread::spawn(move || {
            run_once("printf 'EARLY_OUTPUT\\n'; sleep 30", &writer);
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if captured(&sink).contains("EARLY_OUTPUT") {
                // The clear must precede the bytes it clears for.
                assert!(
                    captured(&sink).starts_with(&String::from_utf8_lossy(CLEAR).into_owned()),
                    "screen must be cleared ahead of the first streamed byte, got {:?}",
                    captured(&sink)
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "output of a still-running command never arrived; got {:?}",
            captured(&sink)
        );
    }

    #[test]
    fn clears_then_writes_the_output_of_a_command_that_exits() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        run_once("printf 'DONE\\n'", &sink);
        assert_eq!(captured(&sink), "\u{1b}[3J\u{1b}[2J\u{1b}[HDONE\n");
    }

    #[test]
    fn clears_the_screen_when_the_command_produces_no_output() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        run_once("true", &sink);
        assert_eq!(captured(&sink), "\u{1b}[3J\u{1b}[2J\u{1b}[H");
    }

    #[test]
    fn streams_stderr_as_well_as_stdout() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        run_once("printf 'ON_STDERR\\n' >&2", &sink);
        assert!(
            captured(&sink).contains("ON_STDERR"),
            "stderr must reach the pane, got {:?}",
            captured(&sink)
        );
    }

    /// A failing command still shows its diagnostics — the exit status is not
    /// a reason to swallow output.
    #[test]
    fn shows_output_of_a_command_that_fails() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        run_once("printf 'BOOM\\n' >&2; exit 3", &sink);
        assert!(
            captured(&sink).contains("BOOM"),
            "failed command's output must reach the pane, got {:?}",
            captured(&sink)
        );
    }
}
