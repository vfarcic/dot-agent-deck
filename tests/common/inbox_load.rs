//! Opt-in CPU pressure owned entirely by the load test, with captured-child cleanup.
use std::process::{Child, Command, Stdio};

pub fn enabled() -> Result<(), String> {
    if std::env::var("DOT_AGENT_DECK_INBOX_LOAD_TEST").as_deref() == Ok("1") {
        Ok(())
    } else {
        Err("inbox load test requires DOT_AGENT_DECK_INBOX_LOAD_TEST=1; the orchestrator schedules this shared-box merge gate".into())
    }
}

struct BusyChild(Child);
impl Drop for BusyChild {
    fn drop(&mut self) {
        // Child::kill targets the captured pid; the shell loop spawns no children.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn busy_command() -> Command {
    let mut command = Command::new("sh");
    command
        .args(["-c", "while :; do :; done"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: after fork this closure performs only prctl/getppid/raise
        // system calls and errno handling, with no locks or heap allocation.
        // Check the captured parent after arming to close the fork/arm race.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::raise(libc::SIGKILL);
                }
                Ok(())
            });
        }
    }
    command
}

pub struct CpuPressure(Vec<BusyChild>);
impl CpuPressure {
    pub fn start() -> Self {
        crate::common::load_context::arm();
        let count = 3 * std::thread::available_parallelism().unwrap().get();
        let mut pressure = Self(Vec::with_capacity(count));
        for _ in 0..count {
            pressure.0.push(BusyChild(
                busy_command()
                    .spawn()
                    .expect("start captured CPU-pressure child"),
            ));
        }
        eprintln!("inbox load: started {count} captured busy-loop children");
        pressure
    }
}
impl Drop for CpuPressure {
    fn drop(&mut self) {
        // Keep the pressure alive while taking the end reading, on success too.
        eprintln!("{}", crate::common::load_context::failure_report());
    }
}
