//! Reusable PTY-spawn primitive shared by the TUI and the daemon.
//!
//! Both the TUI process (`embedded_pane`) and the daemon (`daemon`) need to
//! spawn agent processes attached to a PTY and own the child + master handles
//! for the lifetime of the agent. This module extracts that core so it isn't
//! trapped inside the TUI path. The daemon piece is the foundation for Phase 1
//! (M1.2 streaming attach protocol) — see PRD #76 lines 140–146.

use std::collections::HashMap;
use std::sync::Mutex;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentPtyError {
    #[error("Failed to open PTY: {0}")]
    Open(String),
    #[error("Failed to spawn command: {0}")]
    Spawn(String),
    #[error("Failed to acquire PTY writer: {0}")]
    Writer(String),
    #[error("Failed to clone PTY reader: {0}")]
    Reader(String),
    #[error("Failed to resize PTY: {0}")]
    Resize(String),
    #[error("Agent {0} not found")]
    NotFound(String),
}

/// How to spawn an agent.
pub struct SpawnOptions<'a> {
    /// Command to run. `None` falls back to `$SHELL`. Strings containing spaces
    /// are routed through `$SHELL -c <cmd>` to mirror the TUI's existing
    /// behavior.
    pub command: Option<&'a str>,
    /// Working directory for the spawned process.
    pub cwd: Option<&'a str>,
    /// Initial PTY size.
    pub rows: u16,
    pub cols: u16,
    /// Extra environment variables to inject (e.g. `DOT_AGENT_DECK_PANE_ID`).
    pub env: Vec<(String, String)>,
}

impl Default for SpawnOptions<'_> {
    fn default() -> Self {
        Self {
            command: None,
            cwd: None,
            rows: 24,
            cols: 80,
            env: Vec::new(),
        }
    }
}

/// A spawned agent and the handles needed to keep it alive, write to it, read
/// from it, and resize it. Callers are responsible for explicit cleanup when
/// shutting an agent down — there's no `Drop` impl, since some callers
/// (e.g. `embedded_pane`) destructure these fields and store them
/// individually. The registry uses [`force_kill_and_wait`] (SIGKILL) when it
/// owns whole `AgentPty` values, and [`PtyGuard`] to keep the spawn path
/// leak-free between `spawn()` and registry insertion.
pub struct AgentPty {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub reader: Box<dyn std::io::Read + Send>,
}

/// Forcefully terminate the child and reap it.
///
/// `portable_pty::Child::kill()` sends SIGHUP, which a shell can ignore (some
/// distros' bash/zsh configurations do exactly that), leaving the subsequent
/// `wait()` to block forever. SIGKILL cannot be caught or ignored, so the
/// kernel will tear the process down and `wait()` returns promptly. The
/// master/writer/reader handles are dropped first so any I/O blocked on the
/// PTY unblocks before we wait.
fn force_kill_and_wait(pty: &mut AgentPty) {
    if let Some(pid) = pty.child.process_id() {
        // SAFETY: `kill(2)` is async-signal-safe; sending SIGKILL to a pid we
        // just learned from `process_id()` cannot affect any other process
        // until the kernel reaps the child below.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    } else {
        // No PID exposed — fall back to portable-pty's signaller.
        let _ = pty.child.kill();
    }
    let _ = pty.child.wait();
}

/// RAII guard that owns an `AgentPty` until ownership is explicitly handed
/// off via [`PtyGuard::take`]. If the guard is dropped while still holding
/// the `AgentPty` (e.g. because a panic unwinds the spawn path before
/// insertion into the registry), the child is force-killed and reaped.
struct PtyGuard {
    pty: Option<AgentPty>,
}

impl PtyGuard {
    fn new(pty: AgentPty) -> Self {
        Self { pty: Some(pty) }
    }

    fn take(mut self) -> AgentPty {
        self.pty.take().expect("PtyGuard already taken")
    }
}

impl Drop for PtyGuard {
    fn drop(&mut self) {
        if let Some(mut pty) = self.pty.take() {
            force_kill_and_wait(&mut pty);
        }
    }
}

/// Spawn a new PTY-attached child process.
pub fn spawn(opts: SpawnOptions<'_>) -> Result<AgentPty, AgentPtyError> {
    let pty_system = NativePtySystem::default();

    let pair = pty_system
        .openpty(PtySize {
            rows: opts.rows,
            cols: opts.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AgentPtyError::Open(e.to_string()))?;

    let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut cmd = match opts.command {
        Some(c) if c.contains(' ') => {
            let mut cb = CommandBuilder::new(&default_shell);
            cb.arg("-c");
            cb.arg(c);
            cb
        }
        Some(c) => CommandBuilder::new(c),
        None => CommandBuilder::new(&default_shell),
    };

    if let Some(dir) = opts.cwd {
        cmd.cwd(dir);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AgentPtyError::Spawn(e.to_string()))?;

    // Drop the slave — we interact through the master side only.
    drop(pair.slave);

    let writer = pair
        .master
        .take_writer()
        .map_err(|e| AgentPtyError::Writer(e.to_string()))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| AgentPtyError::Reader(e.to_string()))?;

    Ok(AgentPty {
        child,
        master: pair.master,
        writer,
        reader,
    })
}

/// In-process registry of agent PTYs owned by the daemon. M1.1 only exposes
/// the in-process API; the wire protocol that drives it (`start-agent`,
/// `stop-agent`, `attach-stream`, …) lands in M1.2.
pub struct AgentPtyRegistry {
    inner: Mutex<RegistryInner>,
}

struct RegistryInner {
    next_id: u64,
    agents: HashMap<String, AgentPty>,
}

impl Default for AgentPtyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPtyRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner {
                next_id: 1,
                agents: HashMap::new(),
            }),
        }
    }

    /// Spawn a new agent and return its registry id.
    pub fn spawn_agent(&self, opts: SpawnOptions<'_>) -> Result<String, AgentPtyError> {
        // Hold the freshly spawned PTY in a guard so a panic between here and
        // the `agents.insert` below (e.g. lock poisoning) cannot orphan the
        // child.
        let guard = PtyGuard::new(spawn(opts)?);
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id.to_string();
        inner.next_id += 1;
        inner.agents.insert(id.clone(), guard.take());
        Ok(id)
    }

    /// Stop an agent: SIGKILL the child, reap it, drop its handles.
    pub fn close_agent(&self, id: &str) -> Result<(), AgentPtyError> {
        let mut pty = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .agents
                .remove(id)
                .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?
        };
        force_kill_and_wait(&mut pty);
        Ok(())
    }

    /// All currently-owned agent ids, sorted ascending.
    pub fn agent_ids(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut ids: Vec<String> = inner.agents.keys().cloned().collect();
        ids.sort_by_key(|id| id.parse::<u64>().unwrap_or(0));
        ids
    }

    /// Number of agents currently owned by the registry.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().agents.is_empty()
    }

    /// SIGKILL every agent and drain the registry. Idempotent.
    pub fn shutdown_all(&self) {
        let agents: Vec<AgentPty> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, pty)| pty).collect()
        };
        for mut pty in agents {
            force_kill_and_wait(&mut pty);
        }
    }
}

impl Drop for AgentPtyRegistry {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_default_shell_works() {
        let pty = spawn(SpawnOptions::default()).expect("spawn should succeed");
        let mut child = pty.child;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn registry_spawn_and_close() {
        let registry = AgentPtyRegistry::new();
        assert!(registry.is_empty());

        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("/bin/sh"),
                ..SpawnOptions::default()
            })
            .expect("spawn should succeed");

        assert_eq!(registry.len(), 1);
        assert_eq!(registry.agent_ids(), vec![id.clone()]);

        registry.close_agent(&id).expect("close should succeed");
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_close_unknown_errors() {
        let registry = AgentPtyRegistry::new();
        assert!(matches!(
            registry.close_agent("does-not-exist"),
            Err(AgentPtyError::NotFound(_))
        ));
    }

    #[test]
    fn registry_assigns_sequential_ids() {
        let registry = AgentPtyRegistry::new();
        let id1 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let id2 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let n1: u64 = id1.parse().unwrap();
        let n2: u64 = id2.parse().unwrap();
        assert_eq!(n2, n1 + 1);
        registry.shutdown_all();
    }

    /// Returns true if `kill(pid, 0)` reports the process is gone (ESRCH).
    /// `kill(pid, 0)` performs an existence check without actually signalling.
    fn pid_is_dead(pid: u32) -> bool {
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return false;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::ESRCH)
    }

    #[test]
    fn registry_shutdown_all_clears_state() {
        let registry = AgentPtyRegistry::new();
        let id1 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        let id2 = registry.spawn_agent(SpawnOptions::default()).unwrap();
        assert_eq!(registry.len(), 2);

        // Capture child PIDs so we can verify they're actually gone after
        // shutdown_all (not just absent from the registry map).
        let pids: Vec<u32> = {
            let inner = registry.inner.lock().unwrap();
            [&id1, &id2]
                .into_iter()
                .map(|id| inner.agents.get(id).unwrap().child.process_id().unwrap())
                .collect()
        };

        registry.shutdown_all();
        assert!(registry.is_empty());

        for pid in &pids {
            assert!(
                pid_is_dead(*pid),
                "pid {pid} should be dead after shutdown_all"
            );
        }

        // Idempotent.
        registry.shutdown_all();
    }

    #[test]
    fn registry_drop_kills_agents() {
        // Constructing-and-dropping a registry with a live agent must not
        // hang and must terminate the child. We capture the PID before the
        // registry goes out of scope, then verify the kernel reaped it.
        let pid: u32;
        {
            let registry = AgentPtyRegistry::new();
            let id = registry.spawn_agent(SpawnOptions::default()).unwrap();
            pid = registry
                .inner
                .lock()
                .unwrap()
                .agents
                .get(&id)
                .unwrap()
                .child
                .process_id()
                .unwrap();
        }
        assert!(pid_is_dead(pid), "pid {pid} should be dead after Drop");
    }

    #[test]
    fn spawn_options_env_reaches_child() {
        // Spawn a shell that exits with a status determined by a value passed
        // through SpawnOptions::env. If the env var fails to propagate, the
        // child exits 99 instead of 42 and the assertion below fires.
        let pty = spawn(SpawnOptions {
            command: Some("sh -c 'exit ${DOT_AGENT_DECK_PANE_ID:-99}'"),
            env: vec![("DOT_AGENT_DECK_PANE_ID".into(), "42".into())],
            ..SpawnOptions::default()
        })
        .expect("spawn should succeed");
        let mut child = pty.child;
        let status = child.wait().expect("wait should succeed");
        assert_eq!(
            status.exit_code(),
            42,
            "child did not see DOT_AGENT_DECK_PANE_ID env var"
        );
    }
}
