//! Reusable PTY-spawn primitive shared by the TUI and the daemon.
//!
//! Both the TUI process (`embedded_pane`) and the daemon (`daemon`) need to
//! spawn agent processes attached to a PTY and own the child + master handles
//! for the lifetime of the agent. This module extracts that core so it isn't
//! trapped inside the TUI path. The daemon piece is the foundation for Phase 1
//! (M1.2 streaming attach protocol) — see PRD #76 lines 140–146.

use std::collections::{HashMap, VecDeque};
use std::io::Read as _;
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

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

/// Forcefully terminate the child and reap it. SIGKILL is preferred over
/// `portable_pty::Child::kill()` (which sends SIGHUP) because a shell can
/// ignore SIGHUP — some distros' bash/zsh configurations do exactly that —
/// leaving the subsequent `wait()` to block forever. SIGKILL cannot be
/// caught or ignored, so the kernel tears the process down and `wait()`
/// returns promptly. Callers should drop the master/writer/reader handles
/// before invoking this so any I/O blocked on the PTY unblocks first.
fn force_kill_child_and_wait(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    if let Some(pid) = child.process_id() {
        // SAFETY: `kill(2)` is async-signal-safe; sending SIGKILL to a pid we
        // just learned from `process_id()` cannot affect any other process
        // until the kernel reaps the child below.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if rc != 0 {
            // Log so a weakened cleanup guarantee is observable. ESRCH on an
            // already-reaped child is benign; anything else means the child
            // may outlive us.
            let err = std::io::Error::last_os_error();
            tracing::warn!(pid, error = %err, "SIGKILL failed in force_kill_child_and_wait");
        }
    } else {
        // No PID exposed — fall back to portable-pty's signaller.
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn force_kill_and_wait(pty: &mut AgentPty) {
    force_kill_child_and_wait(&mut pty.child);
}

/// RAII guard that owns a freshly-spawned child between the `spawn_command`
/// call and the point at which ownership is handed off to an [`AgentPty`].
/// If the guard is dropped while still holding the child (e.g. because a
/// later step in [`spawn`] like `take_writer` or `try_clone_reader` returned
/// an error, or a panic unwound through the spawn path), the child is
/// force-killed and reaped so no orphan process is left behind.
struct ChildGuard {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl ChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self { child: Some(child) }
    }

    fn take(mut self) -> Box<dyn portable_pty::Child + Send + Sync> {
        self.child.take().expect("ChildGuard already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            force_kill_child_and_wait(&mut child);
        }
    }
}

/// RAII guard that owns a fully-built `AgentPty` until ownership is handed
/// off via [`PtyGuard::take`]. Used by the registry to cover the gap between
/// [`spawn`] returning an `AgentPty` and the registry's internal `insert`,
/// where a panic (e.g. from lock poisoning) would otherwise drop the
/// `AgentPty` on the floor without killing the child (`AgentPty` has no
/// `Drop` of its own — see the type docs).
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

    // Wrap the freshly-spawned child in an RAII guard *before* any fallible
    // step below: a failure in `take_writer` / `try_clone_reader` (or a
    // panic between them) would otherwise orphan the child. The guard is
    // taken on the success path and its child moved into the AgentPty.
    let child_guard = ChildGuard::new(child);

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
        child: child_guard.take(),
        master: pair.master,
        writer,
        reader,
    })
}

/// Cap on the per-agent scrollback buffer (bytes). Keeps reattach affordable
/// without unbounded memory growth — when a fresh client subscribes, the
/// daemon emits this many recent bytes as the initial render before live
/// output resumes. 1 MiB comfortably covers a typical TUI screen plus a few
/// scrollback pages; the policy is "ring buffer, evict oldest on overflow".
const SCROLLBACK_CAP_BYTES: usize = 1024 * 1024;

/// Capacity of the per-agent broadcast channel for live PTY output. Lossy
/// by design (tokio broadcast semantics) — a slow subscriber that lags past
/// this many messages observes `RecvError::Lagged` and is disconnected by
/// the protocol layer (the client can reattach and replay the snapshot).
const BROADCAST_CAPACITY: usize = 4096;

/// Per-agent broadcast bus. Producers (the reader thread) atomically append
/// to scrollback and publish to subscribers under the same lock so a fresh
/// subscriber's `(snapshot, receiver)` is always consistent: the snapshot
/// covers everything written before the subscriber attached, and the
/// receiver delivers everything written after — no duplicates, no gaps.
pub struct AgentBus {
    tx: broadcast::Sender<Arc<Vec<u8>>>,
    state: Mutex<AgentBusState>,
}

struct AgentBusState {
    scrollback: VecDeque<u8>,
}

impl Default for AgentBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentBus {
    pub fn new() -> Self {
        let (tx, _rx0) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            tx,
            state: Mutex::new(AgentBusState {
                scrollback: VecDeque::new(),
            }),
        }
    }

    /// Append bytes to scrollback and publish to subscribers. Held under the
    /// same lock that subscribers use to take their initial snapshot, so a
    /// concurrent `subscribe` can never split a write between snapshot and
    /// live receiver.
    fn push(&self, data: Vec<u8>) {
        let arc = Arc::new(data);
        let mut state = self.state.lock().unwrap();
        for &b in arc.iter() {
            state.scrollback.push_back(b);
        }
        while state.scrollback.len() > SCROLLBACK_CAP_BYTES {
            state.scrollback.pop_front();
        }
        // Lossy on purpose: we don't block the reader thread on slow
        // subscribers. `send` returns Err only when there are zero
        // receivers, which is fine — scrollback still has the bytes.
        let _ = self.tx.send(arc);
    }

    /// Atomically take the current scrollback snapshot and a receiver
    /// positioned just past it. See type-level docs for the consistency
    /// guarantee.
    pub fn subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Arc<Vec<u8>>>) {
        let state = self.state.lock().unwrap();
        let snapshot: Vec<u8> = state.scrollback.iter().copied().collect();
        let rx = self.tx.subscribe();
        drop(state);
        (snapshot, rx)
    }

    /// Take just the scrollback snapshot, no subscription.
    pub fn snapshot(&self) -> Vec<u8> {
        self.state
            .lock()
            .unwrap()
            .scrollback
            .iter()
            .copied()
            .collect()
    }
}

/// Reader-thread loop: pull bytes from the PTY master and publish them to
/// the bus. Exits cleanly when the PTY returns EOF (the child was killed or
/// otherwise terminated). The thread is detached — `RunningAgent` does not
/// hold a `JoinHandle` for it because shutdown is driven entirely by closing
/// the PTY (see `AgentPtyRegistry::close_agent`).
fn pump_reader(mut reader: Box<dyn std::io::Read + Send>, bus: Arc<AgentBus>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bus.push(buf[..n].to_vec()),
            Err(_) => break,
        }
    }
}

/// Snapshot of the writer + bus needed to attach a streaming client.
/// Returned by [`AgentPtyRegistry::subscribe`].
pub struct AttachHandle {
    pub snapshot: Vec<u8>,
    pub rx: broadcast::Receiver<Arc<Vec<u8>>>,
    pub writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
}

/// One agent owned by the registry: child + master + shared writer + bus.
/// Field names are stable — tests and tooling that peek into the registry
/// (e.g. for `process_id()`) rely on `child` existing here.
pub struct RunningAgent {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub writer: Arc<AsyncMutex<Box<dyn std::io::Write + Send>>>,
    pub bus: Arc<AgentBus>,
}

/// In-process registry of agent PTYs owned by the daemon. M1.1 only exposed
/// the in-process API; M1.2 wires it to the streaming attach protocol via
/// [`AgentBus`] and [`AttachHandle`].
pub struct AgentPtyRegistry {
    inner: Mutex<RegistryInner>,
}

struct RegistryInner {
    next_id: u64,
    agents: HashMap<String, RunningAgent>,
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
        // Defense in depth: `spawn` already protects the child internally
        // via its own `ChildGuard`, so any failure or panic *inside* spawn
        // cannot orphan the child. This outer `PtyGuard` covers the
        // remaining gap — between `spawn` returning the `AgentPty` and the
        // `agents.insert` below — where lock poisoning on `inner.lock()`
        // would otherwise drop the `AgentPty` without killing the child
        // (`AgentPty` has no `Drop`).
        let guard = PtyGuard::new(spawn(opts)?);
        let mut inner = self.inner.lock().unwrap();

        let pty = guard.take();
        let AgentPty {
            child,
            master,
            writer,
            reader,
        } = pty;

        let bus = Arc::new(AgentBus::new());
        let bus_for_thread = bus.clone();
        // Detached thread: exits when the PTY returns EOF (child killed).
        std::thread::spawn(move || pump_reader(reader, bus_for_thread));

        let agent = RunningAgent {
            child,
            master,
            writer: Arc::new(AsyncMutex::new(writer)),
            bus,
        };

        let id = inner.next_id.to_string();
        inner.next_id += 1;
        inner.agents.insert(id.clone(), agent);
        Ok(id)
    }

    /// Stop an agent: SIGKILL the child, reap it, drop its handles. Any
    /// streaming subscribers will observe their broadcast receiver close
    /// shortly after (once the reader thread sees EOF and drops its bus
    /// reference).
    pub fn close_agent(&self, id: &str) -> Result<(), AgentPtyError> {
        let mut agent = {
            let mut inner = self.inner.lock().unwrap();
            inner
                .agents
                .remove(id)
                .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?
        };
        force_kill_child_and_wait(&mut agent.child);
        Ok(())
    }

    /// Subscribe to an agent's live output and take its scrollback snapshot
    /// in one atomic step. Used by the attach protocol handler.
    pub fn subscribe(&self, id: &str) -> Result<AttachHandle, AgentPtyError> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        let (snapshot, rx) = agent.bus.subscribe();
        Ok(AttachHandle {
            snapshot,
            rx,
            writer: agent.writer.clone(),
        })
    }

    /// Take just the current scrollback snapshot for an agent.
    pub fn snapshot(&self, id: &str) -> Result<Vec<u8>, AgentPtyError> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(id)
            .ok_or_else(|| AgentPtyError::NotFound(id.to_string()))?;
        Ok(agent.bus.snapshot())
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
        let agents: Vec<RunningAgent> = {
            let mut inner = self.inner.lock().unwrap();
            inner.agents.drain().map(|(_, a)| a).collect()
        };
        for mut agent in agents {
            force_kill_child_and_wait(&mut agent.child);
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
    fn child_guard_drop_kills_orphan_child() {
        // Models the leak scenario the in-`spawn()` ChildGuard now covers:
        // a child has been spawned, but a *later* fallible step (the real
        // ones being `take_writer` / `try_clone_reader`) errors out before
        // the child can be moved into the returned AgentPty. Dropping the
        // guard on that error path must force-kill and reap the child so
        // no orphan PID is left behind.
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let cmd = CommandBuilder::new(&default_shell);
        let child = pair.slave.spawn_command(cmd).expect("spawn should succeed");
        drop(pair.slave);
        let pid = child.process_id().expect("child should expose a pid");

        let guard = ChildGuard::new(child);
        // Drop the master *before* the guard so any PTY I/O the child is
        // blocked on unblocks before SIGKILL — matching the production
        // shutdown order.
        drop(pair.master);
        drop(guard);

        assert!(
            pid_is_dead(pid),
            "pid {pid} should be dead after ChildGuard drop"
        );
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
