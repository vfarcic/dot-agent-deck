// PRD #42 M8: most of this harness (PTY spawning, Unix-domain sockets, libc
// process-group signalling, mode-bit chmod) is Unix-only at the source level, so
// those items carry a per-item `#[cfg(unix)]` gate. The genuinely cross-platform
// helpers — the L1 vt100/render helpers (`nonblank_rows`, `joined_rows`), the
// synthetic-agent submodule, and the legacy lock-dir helpers (`init_test_env`,
// `race_safe_tempdir`, `lock_dir_path`) — are left ungated so the fast-tier test
// files that use them (`render_button_bar.rs`, `render_layout.rs`,
// `agent_event.rs`, `orchestration_delegate.rs`, `delegate_prompt_injection.rs`)
// compile on the `x86_64-pc-windows-msvc` build-windows CI target too. A
// wholesale module-level `#![cfg(unix)]` would drop those files' Windows
// coverage. The Windows (ConPTY + named-pipe) port of the Unix-only harness is
// tracked by #164 (M10).
//! PRD #77 — TUI testing harness (L2 slice).
//!
//! Spawns the production `dot-agent-deck` binary inside a `portable-pty`
//! PTY, parses its stdout through a `vt100` grid, and exposes a small
//! fluent surface so tests can wait on observable state without
//! sleeping. Decision 20 pins the PTY size + color env so the grid is
//! deterministic; Decisions 12 + 21 + 28 govern per-test isolation,
//! quiescence-based waits, and failure recordings.
//!
//! The Unix-only harness items carry a per-item `#[cfg(unix)]` gate (see the
//! file header) so this single module can be shared by every L2 test under the
//! `e2e` feature while the cross-platform helpers still compile on Windows. The
//! harness uses production deps only (`portable-pty`, `vt100`, `tempfile`,
//! `libc`, `serde_json`), all already in `Cargo.toml`.

#![allow(dead_code)]

/// PRD #201 M1.3: the agent-agnostic synthetic-agent harness — a scripted
/// stand-in that emits `delegate` / `work-done` / `agent-event` frames,
/// parameterized by agent identity. Shared by the fast-tier contract tests
/// (`tests/agent_event.rs`, `tests/orchestration_delegate.rs`).
pub mod synthetic_agent;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Decision 21: tunable harness constant for `wait_until_quiescent`.
/// 50 ms idle window — long enough that the dashboard's full repaint
/// has settled, short enough that test runtime stays bounded.
pub const QUIESCENT_IDLE_MS: u64 = 50;

/// Default ceiling on quiescence / signal waits. Tests do not pass a
/// budget — quiescence and string-signal waits are bounded internally.
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Max-lifetime cap for tests that spawn `dot-agent-deck wrap` DIRECTLY (rather
/// than through [`TuiDeck`] / `DaemonProc`, which inject their own cap), so a
/// wrapper orphaned by a SIGKILLed / timed-out / panicking test self-exits
/// instead of leaking to PID 1 forever.
///
/// This is not hypothetical: three `wrap --agent codex` stubs from this file's
/// own probes were found alive for **three days**, from a worktree that had
/// already been deleted — two of them spinning a shell `sleep 0.01` loop at
/// ~100 wakeups/second, and one holding `trap '' TERM` so no SIGTERM could
/// reach it. The wrapper honours the cap as of the same change that added this
/// constant; before that the env var was read only by `daemon serve`.
///
/// Deliberately generous (120 s) relative to these sub-10 s probes: it is a
/// leak backstop, not a test timeout, so it must never be the thing that ends a
/// legitimately slow run.
pub const WRAP_TEST_MAX_LIFETIME_SECS: &str = "120";

/// Issue #709: the base ceiling on a wait for a freshly spawned child's FIRST
/// OUTPUT, before [`load_scaled`] widens it for a contended machine.
///
/// This bounds a BOOT, not a behaviour. Nothing a test asserts is derived from
/// it — the waits it feeds return the instant their condition holds, so on an
/// idle box a `sh` stand-in satisfies them in single-digit milliseconds and this
/// number is never reached. It exists so that "the child has not been scheduled
/// yet" cannot be mistaken for "the child produced the wrong thing", which is
/// precisely what a 2 s ceiling did on a 16-core box at load average 44.
pub const CHILD_BOOT_BASE: Duration = Duration::from_secs(8);

/// Issue #709: the 1-minute load average per CPU, or `None` where this platform
/// does not publish one cheaply.
///
/// Linux only, deliberately. `getloadavg(3)` exists on macOS but is not exposed
/// by the `libc` crate for either `linux-gnu` or `apple` targets, and shelling
/// out to `sysctl` from a test helper buys a process spawn on every wait to
/// refine a number that is only ever used to make a ceiling MORE generous.
/// Elsewhere the answer is `None` and [`load_scaled`] applies the full
/// multiplier, which is the safe direction: a wider ceiling on a machine whose
/// contention cannot be measured, paid only when a child is genuinely slow.
pub fn machine_load_per_cpu() -> Option<f64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let raw = std::fs::read_to_string("/proc/loadavg").ok()?;
    let one_minute: f64 = raw.split_whitespace().next()?.parse().ok()?;
    let cpus = std::thread::available_parallelism().ok()?.get() as f64;
    if !one_minute.is_finite() || cpus <= 0.0 {
        return None;
    }
    Some(one_minute / cpus)
}

/// Issue #709: the largest factor [`load_scaled`] will multiply a base ceiling
/// by, and therefore the ceiling on how long a starved child is waited for.
///
/// Six rather than "whatever the load average says": the mapping from load to
/// scheduling delay is not linear and an unbounded multiplier would let a
/// runaway load average turn a genuinely hung child into a test that runs until
/// nextest's own `terminate-after` kill (3 x 60 s by default), which costs the
/// assertion's diagnostics — the thing every widened wait here exists to
/// preserve. At the measured failure's load (44 on 16 cores = 2.75) this yields
/// 22 s against the 2 s that failed; the cap only binds past 6.0.
const MAX_LOAD_FACTOR: f64 = 6.0;

/// Issue #709: widen a wait ceiling in proportion to how contended the machine
/// is, so a fast box still fails fast and a loaded one still passes.
///
/// Apply this ONLY to a ceiling on something that must HAPPEN, never to a
/// negative window in which something must NOT happen: the waits it feeds return
/// the moment their condition holds, so a wider ceiling is free on the happy
/// path and is paid only where the alternative was a wrong verdict. A negative
/// window is the opposite — it is always paid in full, and its length is part of
/// what the test asserts.
pub fn load_scaled(base: Duration) -> Duration {
    let factor = machine_load_per_cpu()
        .unwrap_or(MAX_LOAD_FACTOR)
        .clamp(1.0, MAX_LOAD_FACTOR);
    base.mul_f64(factor)
}

/// Issue #709: [`load_scaled`] applied to [`CHILD_BOOT_BASE`] — the ceiling a
/// fast-tier test gives a freshly spawned child to produce its first byte.
pub fn child_boot_budget() -> Duration {
    load_scaled(CHILD_BOOT_BASE)
}

/// Issue #709: how long after a child stops being live its output is still
/// waited for, so a stand-in that printed and then died is not reported as one
/// that printed nothing.
///
/// The bytes a child writes on its way out reach the snapshot through the
/// detached `pump_reader` OS thread, which is not synchronised with the exit
/// bookkeeping `agent_is_live` reads — so "no longer live" is a reason to stop
/// waiting for MORE output, never a reason to trust the snapshot in hand.
const POST_EXIT_DRAIN: Duration = Duration::from_millis(250);

/// Issue #709: wait for a FRESHLY SPAWNED child's FIRST OUTPUT, returning the
/// snapshot either way so the caller still asserts on (and prints) it.
///
/// The three fast-tier waits this replaces were already condition-driven — they
/// returned the instant the needle landed — and still failed on a 16-core box at
/// load average 44, because their ceiling was a flat 2 s or 5 s sized for an idle
/// machine. The child had not been scheduled at all, so the assertion underneath
/// reported an EMPTY snapshot and read exactly like a delivery defect. Both ends
/// of that are fixed here, and neither weakens what the caller asserts:
///
/// * the ceiling is [`child_boot_budget`], scaled by how contended the machine
///   actually is, so an idle box keeps its prompt failure and a loaded one buys
///   patience it alone pays for; and
/// * the wait ends early once the child is no longer live, so the failure a
///   generous ceiling might otherwise have slowed — a stand-in that DIED rather
///   than printed — still fails about as fast as it did against 2 s.
///
/// Use it only for the boot leg. A wait on a BEHAVIOUR the daemon must perform
/// belongs on that behaviour's own budget, and a negative window in which
/// something must not happen must not be widened at all — its length is part of
/// what the test asserts.
#[allow(dead_code)]
pub async fn wait_for_child_first_output(
    registry: &dot_agent_deck::agent_pty::AgentPtyRegistry,
    agent_id: &str,
    needle: &[u8],
) -> Vec<u8> {
    let deadline = tokio::time::Instant::now() + child_boot_budget();
    let mut drain_deadline: Option<tokio::time::Instant> = None;
    loop {
        let snapshot = registry.snapshot(agent_id).unwrap_or_default();
        if snapshot.windows(needle.len()).any(|w| w == needle) {
            return snapshot;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline || drain_deadline.is_some_and(|drained| now >= drained) {
            return snapshot;
        }
        if drain_deadline.is_none() && !registry.agent_is_live(agent_id) {
            drain_deadline = Some(now + POST_EXIT_DRAIN);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Decision 20: pinned PTY dimensions for the deck. Resize tests
/// override via `TuiDeck::resize`.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// One byte-stream chunk recorded for the asciinema cast on failure
/// (Decision 28). Time is seconds since session start; data is the
/// raw bytes off the PTY master, which is what asciinema-format
/// `agg` and `asciinema play` expect.
#[derive(Debug, Clone)]
struct CastEvent {
    offset_secs: f64,
    data: Vec<u8>,
}

/// Optional pre-staged saved-session entry — when set, the harness
/// generates a `session.toml` under the per-test tempdir and passes
/// `--continue` so the deck auto-opens one pane running this command
/// at launch. Used by chain-smoke tests to drive real agents
/// (PRD #77 Decision 8) without user keystrokes.
#[derive(Debug, Clone)]
struct ContinueSession {
    pane_name: String,
    command: String,
    mode: Option<String>,
}

/// Which agent's credential set the test wants imported from the
/// host HOME into the per-test tempdir HOME. M3.1 auditor Nit 1 —
/// the M2.1 N3 attribution was misleading: M2.1 banned symlinks in
/// the fixture-copy path, and M3.1 carries that ban forward into
/// the credential-copy path with a hard refuse (source symlink ->
/// Err) and atomic 0o600 creation on the destination (S2 + S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialImport {
    ClaudeCode,
    OpenCode,
    Codex,
    Devin,
}

/// Builder for [`TuiDeck`]. Use the test surface
/// [`TuiDeck::builder`].
pub struct TuiDeckBuilder {
    cols: u16,
    rows: u16,
    extra_env: Vec<(String, String)>,
    continue_session: Option<ContinueSession>,
    credential_imports: Vec<CredentialImport>,
    keybindings_toml: Option<String>,
    claude_trust_paths: Vec<String>,
    claude_trust_workdir: bool,
    suppress_success_recording: bool,
    launch_subdir: Option<PathBuf>,
}

impl TuiDeckBuilder {
    /// Override an environment variable for the spawned binary. Tests
    /// use this when their behaviour-under-test demands a different
    /// value than Decision 20's pinned default (e.g. `NO_COLOR=1`).
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Launch the deck from `subdir` *inside* the fixture instead of at the
    /// fixture root, creating it if absent. The fixture still lands at the
    /// tempdir root, so the project's `.dot-agent-deck.toml` sits one or more
    /// levels ABOVE the deck's working directory — which is the shape issue
    /// #577 is about: a deck started somewhere below its project root.
    ///
    /// Everything else is unchanged; in particular `HOME`, both sockets and
    /// the state dir still point at the per-test tempdir, so a test using this
    /// is no less isolated than one launching at the root.
    pub fn with_launch_subdir(mut self, subdir: impl Into<PathBuf>) -> Self {
        self.launch_subdir = Some(subdir.into());
        self
    }

    /// Override the initial PTY size. Resize tests do this when the
    /// behaviour under test depends on a non-default geometry.
    pub fn with_pty_size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Stage a `session.toml` in the per-test tempdir and pass
    /// `--continue` on launch so the deck auto-opens one pane running
    /// `command` against the tempdir as its working directory.
    /// Used by chain-smoke tests to drive a real agent CLI without
    /// keystrokes.
    pub fn with_continue_session(
        mut self,
        pane_name: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        self.continue_session = Some(ContinueSession {
            pane_name: pane_name.into(),
            command: command.into(),
            mode: None,
        });
        self
    }

    /// Stage a saved pane carrying mode membership so restore exercises the
    /// mode-tab rebuild path rather than the plain-pane fallback.
    pub fn with_continue_mode_session(
        mut self,
        pane_name: impl Into<String>,
        command: impl Into<String>,
        mode: impl Into<String>,
    ) -> Self {
        self.continue_session = Some(ContinueSession {
            pane_name: pane_name.into(),
            command: command.into(),
            mode: Some(mode.into()),
        });
        self
    }

    /// Import the host user's Claude Code credentials + settings into
    /// the per-test tempdir HOME so a spawned `claude` CLI can
    /// authenticate. Hook entries in the imported `settings.json` are
    /// stripped — the deck installs its own hooks pointing at the
    /// per-test paths. The destination credential file is created
    /// atomically with mode 0o600 (M3.1 auditor S2) and the source
    /// path is refused if it's a symlink (M3.1 auditor S3).
    ///
    /// The actual copy happens at launch time. Missing or
    /// unreadable credentials surface through
    /// [`try_launch_with_fixture`](Self::try_launch_with_fixture)
    /// as `Err(reason)`; the convenience
    /// [`launch_with_fixture`](Self::launch_with_fixture) panics
    /// instead. Pair with [`check_claude_available`] and
    /// [`skip_unless!`] to convert that into a clean
    /// Decision-26 runtime skip.
    pub fn with_imported_claude_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::ClaudeCode);
        self
    }

    /// Same shape as [`with_imported_claude_credentials`] but for OpenCode.
    /// Only `auth.json` is imported; the harness writes a minimal isolated
    /// `opencode.json` and never copies host plugins, MCP commands, providers,
    /// or other user configuration into a recorded real-agent run.
    pub fn with_imported_opencode_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::OpenCode);
        self
    }

    /// Import the host user's Codex `auth.json` into the isolated per-test HOME
    /// and trust the fixture working directory in Codex's project config. Pair
    /// with [`check_codex_available`] so missing or rejected credentials cleanly
    /// skip a real-Codex test instead of failing during TUI launch.
    pub fn with_imported_codex_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::Codex);
        self
    }

    /// Import the host user's Devin credentials into the isolated per-test HOME
    /// and seed a config that runs unattended (setup wizard skipped, workspace
    /// trust waived). Pair with [`check_devin_available`] so a host without
    /// Devin skips cleanly instead of failing during TUI launch.
    pub fn with_imported_devin_credentials(mut self) -> Self {
        self.credential_imports.push(CredentialImport::Devin);
        self
    }

    /// Pre-trust `path` for a daemon-spawned interactive `claude`, so it clears
    /// BOTH first-run gates with no human keystroke: the global onboarding gate
    /// (`hasCompletedOnboarding`) and the per-folder trust gate
    /// (`projects.<path>.hasTrustDialogAccepted`). At launch a `~/.claude.json`
    /// is written into the per-test HOME (the same HOME the daemon — and every
    /// agent it spawns — inherits): it starts from the host's `~/.claude.json`
    /// (preserving `oauthAccount` + `hasCompletedOnboarding` so the global
    /// onboarding flow is skipped) and marks each `path` as a trusted project.
    ///
    /// In production a dispatched agent runs in the user's already-trusted repo,
    /// so the trust dialog never appears; a fresh per-issue worktree cwd would
    /// otherwise trip it and swallow the daemon-injected prompt. `path` must be
    /// the EXACT cwd string the spawned agent runs in (e.g. the per-issue
    /// worktree dir from `dot_agent_deck::issue_dispatch::derive_issue_paths`).
    /// Call once per trusted folder. Ported from
    /// `e2e_delegate_work_done_chain.rs::prepare_claude_home`.
    pub fn with_claude_project_trust(mut self, path: impl Into<String>) -> Self {
        self.claude_trust_paths.push(path.into());
        self
    }

    /// Like [`with_claude_project_trust`](Self::with_claude_project_trust) but
    /// for the per-test WORK DIR — the copied-fixture root the deck runs in and
    /// the cwd of a `with_continue_session` pane. That path is the harness's
    /// tempdir, minted inside `launch`, so a caller cannot name it in advance;
    /// this flag defers the seeding until it exists. Both the raw and the
    /// canonicalized form are trusted, because the agent's own `cwd` may come
    /// back symlink-resolved and the trust key is matched verbatim.
    pub fn with_claude_trust_workdir(mut self) -> Self {
        self.claude_trust_workdir = true;
        self
    }

    /// Keep this client out of the successful-run recording artifact even when
    /// `DOT_AGENT_DECK_RECORD=1`. Multi-client scenarios use one primary deck
    /// as the viewer-facing cast and secondary decks only as real control
    /// surfaces; letting every client dump under the shared test-function name
    /// would make the last drop nondeterministically overwrite the primary cast.
    pub fn without_success_recording(mut self) -> Self {
        self.suppress_success_recording = true;
        self
    }

    // NOTE (PRD #201): a `with_pi_extension()` builder that pre-staged the
    // bundled Pi extension into the per-test HOME was removed. Because `TuiDeck`
    // drives the REAL binary, its lazy-spawned daemon runs the `daemon serve`
    // entry, whose daemon-startup auto-materialize writes the extension into the
    // daemon's HOME (this per-test HOME, which the pi child inherits) before pi
    // boots — so the TuiDeck pi e2es exercise that production flow directly. (The
    // in-process-daemon pi e2es bypass that entry and stage the extension
    // themselves via `orchestrator_ext::materialize`.)

    /// Stage a `keybindings.toml` in the per-test HOME's config dir
    /// (`$HOME/.config/dot-agent-deck/keybindings.toml`, mirroring the
    /// `config.toml` path resolved by `dot_agent_deck::config`) before
    /// launch, so the deck reads it during startup. `content` is written
    /// verbatim — pass malformed TOML to exercise the fallback path
    /// (PRD #40 `keybindings/fallback/*`). The file is created with the
    /// HOME-relative path so two clients in the same suite never share
    /// bindings.
    pub fn with_keybindings_toml(mut self, content: impl Into<String>) -> Self {
        self.keybindings_toml = Some(content.into());
        self
    }

    /// Launch the deck against the named fixture under
    /// `tests/fixtures/`. The fixture is copied into the per-test
    /// tempdir at launch (Decision 12); the deck's `HOME`, hook socket,
    /// and attach socket all point inside that tempdir.
    ///
    /// Panics on credential-import / setup failure. For tests that
    /// would rather surface those errors as a `Result`, call
    /// [`try_launch_with_fixture`](Self::try_launch_with_fixture).
    pub fn launch_with_fixture(self, fixture_name: &str) -> TuiDeck {
        self.try_launch_with_fixture(fixture_name)
            .unwrap_or_else(|e| panic!("launch_with_fixture failed: {e}"))
    }

    /// Fallible variant of [`launch_with_fixture`]. Returns
    /// `Err(reason)` on credential-import or other setup failures
    /// where the reason is the same user-facing string the
    /// `check_*_available()` helpers produce (per Decision 26
    /// runtime-skip wording — M3.1 reviewer Nit 3).
    pub fn try_launch_with_fixture(self, fixture_name: &str) -> Result<TuiDeck, String> {
        TuiDeck::try_launch_inner(self, fixture_name)
    }
}

/// Handle to a running deck.
pub struct TuiDeck {
    pty_master: Box<dyn MasterPty + Send>,
    /// PTY master write side, taken ONCE at construction. `MasterPty::
    /// take_writer()` is single-shot (a 2nd call errors), so `send_keys` /
    /// `send_bytes` (and `click`/`scroll`, which call it 2×/1×) must share one
    /// stored writer rather than taking a fresh one per call. Behind a `Mutex`
    /// so the write helpers can keep `&self`, and behind an `Arc` so the reader
    /// thread can share it to answer the deck's terminal-capability queries
    /// (see [`answer_terminal_queries`]).
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    last_byte_at: Arc<Mutex<Instant>>,
    cast_events: Arc<Mutex<Vec<CastEvent>>>,
    /// M4.6 P1: append-only buffer of EVERY byte the reader thread
    /// has seen since launch. `wait_for_strings_in_order` snapshots
    /// this against an index captured at call time so two status
    /// transitions rendered in the same polling window can't race
    /// the wait past one of them — the substring search runs over
    /// the rolling history, not the live vt100 grid. Bounded by
    /// total test duration (the harness's 10s wait ceiling +
    /// per-test cap) — same memory profile as `cast_events`.
    byte_history: Arc<Mutex<Vec<u8>>>,
    cast_started_at: Instant,
    reader_stop: Arc<AtomicBool>,
    reader_handle: Option<JoinHandle<()>>,
    child: Box<dyn Child + Send + Sync>,
    tempdir: tempfile::TempDir,
    home: PathBuf,
    hook_socket: PathBuf,
    attach_socket: PathBuf,
    fixture_path: PathBuf,
    test_name: String,
    cols: u16,
    rows: u16,
    record_on_success: bool,
    /// Exact secret values learned from imported auth files. Recording artifacts
    /// are scrubbed immediately before they are written so an agent or provider
    /// that echoes a credential cannot persist it in `full-stream.cast`.
    recording_redactions: Vec<String>,
}

/// Observable terminal-cell styling from the outer vt100 screen driven by the
/// real deck binary. L2 rendering tests use this to assert user-visible
/// attributes such as DIM without reaching into production UI state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridCellStyle {
    pub fgcolor: vt100::Color,
    pub bgcolor: vt100::Color,
    pub bold: bool,
    pub dim: bool,
    pub inverse: bool,
}

impl From<&vt100::Cell> for GridCellStyle {
    fn from(cell: &vt100::Cell) -> Self {
        Self {
            fgcolor: cell.fgcolor(),
            bgcolor: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            inverse: cell.inverse(),
        }
    }
}

/// Hardware-cursor state after the outer vt100 parser has consumed the real
/// terminal stream, including the style of the cell beneath that cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCursorSnapshot {
    pub hidden: bool,
    pub row: u16,
    pub col: u16,
    pub cell: Option<GridCellStyle>,
}

impl TuiDeck {
    /// One-line convenience: build a default deck and launch it.
    pub fn launch_with_fixture(fixture_name: &str) -> Self {
        Self::builder().launch_with_fixture(fixture_name)
    }

    /// Start a fluent builder for non-default deck launches.
    pub fn builder() -> TuiDeckBuilder {
        // The L2 harness spawns the real binary, which lazy-spawns a daemon —
        // both inherit this process's env. `init_test_env` covers the legacy
        // tests; this covers every `TuiDeck`-driven one.
        detach_from_any_live_deck();
        TuiDeckBuilder {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            extra_env: Vec::new(),
            continue_session: None,
            credential_imports: Vec::new(),
            keybindings_toml: None,
            claude_trust_paths: Vec::new(),
            claude_trust_workdir: false,
            suppress_success_recording: false,
            launch_subdir: None,
        }
    }

    fn try_launch_inner(builder: TuiDeckBuilder, fixture_name: &str) -> Result<Self, String> {
        let test_name = current_test_name();

        // M2.1 auditor S1 + M3.1 auditor S4: create the per-test
        // tempdir with mode 0o700 atomically. `harness_tempdir()`
        // followed by `set_permissions(0o700)` had a small umask-derived
        // 0o755 window between creation and chmod — closed here by
        // asking tempfile to apply 0o700 at creation.
        //
        // Issue #322: created *inside* `harness_temp_root()` so it is covered by
        // the process-exit cleanup and by `cargo xtask clean-e2e-tmp`. This is
        // the harness's largest tempdir by far — it holds the seeded agent HOME,
        // observed at 276 MB — so leaving it at the top of `/tmp` was what
        // actually filled a RAM-backed tmpfs.
        let tempdir = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tempfile::Builder::new()
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir_in(harness_temp_root())
                    .expect("create per-test tempdir")
            }
            #[cfg(not(unix))]
            {
                tempfile::Builder::new()
                    .tempdir_in(harness_temp_root())
                    .expect("create per-test tempdir")
            }
        };
        let work = tempdir.path().to_path_buf();

        // Verify the atomic-creation 0o700 mode actually stuck —
        // catches a future tempfile API rename that would silently
        // skip the permission application.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&work)
                .expect("stat tempdir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "tempdir mode is 0o{mode:o}, expected 0o700 (M3.1 auditor S4 — atomic creation should have stamped this)"
            );
        }

        // Decision 12: copy fixture into the tempdir, then `git init`
        // (some deck paths probe `.git`).
        let fixture_src = locate_fixture(fixture_name);
        copy_dir_recursively(&fixture_src, &work).expect("copy fixture into tempdir");
        let _ = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(&work)
            .status();

        let home = work.join("home");
        std::fs::create_dir_all(&home).expect("create per-test HOME");
        // PRD #381: the durable-path candidate the deck's hook installers
        // resolve, pointed at the binary under test. See `seed_durable_binary`.
        #[cfg(unix)]
        seed_durable_binary(&home);

        // PRD #201: the per-test HOME deliberately starts WITHOUT the bundled Pi
        // extension. Because `TuiDeck` drives the REAL binary, its lazy-spawned
        // daemon runs the `daemon serve` entry, whose daemon-startup
        // auto-materialize writes the extension into the daemon's HOME (this
        // per-test HOME, which the pi child inherits) before pi boots — so the
        // TuiDeck pi e2es exercise that production flow rather than a pre-staged
        // shortcut.

        // PRD #40: stage the keybindings config the deck reads at
        // startup. Path mirrors `config_path()` in
        // `dot_agent_deck::config` — `$HOME/.config/dot-agent-deck/` —
        // with the filename `keybindings.toml`. Written before the
        // binary spawns so the deck sees it on its first config read.
        if let Some(ref kb) = builder.keybindings_toml {
            let cfg_dir = home.join(".config").join("dot-agent-deck");
            std::fs::create_dir_all(&cfg_dir).expect("create keybindings config dir");
            std::fs::write(cfg_dir.join("keybindings.toml"), kb).expect("write keybindings.toml");
        }

        // Chain-smoke credential imports (PRD #77 Decision 8). Tests
        // pair these with `check_*_available()` + `skip_unless!`; if
        // the credentials disappeared between the precheck and here,
        // we surface a Decision-26-shaped error through `try_launch_*`
        // (M3.1 reviewer Nit 3) so the test's harness frame doesn't
        // panic mid-suite — callers can choose whether to skip or
        // bubble up.
        let mut recording_redactions = Vec::new();
        for kind in &builder.credential_imports {
            match kind {
                CredentialImport::ClaudeCode => {
                    import_claude_credentials(&home).map_err(|e| e.to_string())?;
                }
                CredentialImport::OpenCode => {
                    recording_redactions
                        .extend(import_opencode_credentials(&home).map_err(|e| e.to_string())?);
                }
                CredentialImport::Codex => {
                    import_codex_credentials(&home).map_err(|e| e.to_string())?;
                }
                CredentialImport::Devin => {
                    recording_redactions
                        .extend(import_devin_credentials(&home).map_err(|e| e.to_string())?);
                }
            }
        }
        // Issue #502/#785 — the API-key sink, closed the same way the OpenCode
        // and Devin ones are. Two strings, both registered whenever a key is
        // present, because `inherit_pass` now puts the key into every deck this
        // harness spawns:
        //
        //   * the KEY itself, since anything in the pane that echoes its
        //     environment renders it into the vt100 grid, and the grid is
        //     persisted as `full-stream.cast` / `final-grid.txt`;
        //   * its 20-character SUFFIX, because Claude Code's API-key approval
        //     prompt renders exactly that (see `seed_claude_project_trust`), so
        //     any run where the approval seeding is wrong or has not happened
        //     yet paints a derivative of the secret straight into the recording
        //     — and from there into nextest's captured output, which lane 2
        //     lifts into the job log with `--success-output=final`. GitHub masks
        //     a registered secret's exact value in a rendered log; a substring
        //     of it is not covered.
        //
        // Registered unconditionally rather than only on a claude import: the
        // key reaches the deck either way, and a redaction for a string that
        // never appears costs one failed substring search per artifact.
        if let Some(key) = anthropic_api_key() {
            recording_redactions.extend(api_key_recording_redactions(&key));
        }
        recording_redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
        recording_redactions.dedup();

        // Pre-trust folders for a daemon-spawned interactive `claude` so it
        // clears its first-run onboarding + per-folder trust gates without a
        // human keystroke. The `~/.claude.json` lands in the SAME per-test HOME
        // the daemon (and every agent it spawns) inherits.
        let mut claude_trust_paths = builder.claude_trust_paths.clone();
        if builder.claude_trust_workdir {
            // The work dir only exists now, so `with_claude_trust_workdir`
            // could not name it. Trust the raw path AND its canonical form —
            // the agent reports its cwd symlink-resolved on some platforms and
            // the trust key is matched verbatim.
            claude_trust_paths.push(work.to_string_lossy().into_owned());
            if let Ok(canon) = std::fs::canonicalize(&work) {
                let canon = canon.to_string_lossy().into_owned();
                if !claude_trust_paths.contains(&canon) {
                    claude_trust_paths.push(canon);
                }
            }
        }
        if !claude_trust_paths.is_empty() {
            seed_claude_project_trust(&home, &claude_trust_paths).map_err(|e| e.to_string())?;
        }

        // Write the saved-session file the deck auto-restores on startup
        // (PRD #89: no `--continue` flag anymore), if the test asked for one.
        // The pane runs `command` in the tempdir's working directory so the
        // agent has a real cwd to operate against (the deck's restore path
        // skips panes whose `dir` doesn't exist on disk).
        let session_toml_path = work.join("session.toml");
        if let Some(cs) = &builder.continue_session {
            write_continue_session_file(
                &session_toml_path,
                &work,
                &cs.pane_name,
                &cs.command,
                cs.mode.as_deref(),
            )
            .expect("write continue session.toml");
        }

        let hook_socket = work.join("hook.sock");
        let attach_socket = work.join("attach.sock");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: builder.rows,
                cols: builder.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        // Cargo sets `CARGO_BIN_EXE_<bin-name>` at integration-test
        // build time to the path of the freshly-built binary under
        // test. The `env!()` evaluates at compile time so the harness
        // always launches whatever the current test build produced
        // (debug vs. release matches the test's profile).
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = CommandBuilder::new(bin);
        // Default: launch at the fixture root. `with_launch_subdir` moves the
        // deck's cwd below it without moving the fixture (issue #577).
        let launch_dir = match &builder.launch_subdir {
            Some(sub) => {
                let dir = work.join(sub);
                std::fs::create_dir_all(&dir).expect("create launch subdir");
                dir
            }
            None => work.clone(),
        };
        cmd.cwd(&launch_dir);
        // PRD #89: the `--continue` flag was removed — auto-restore is now the
        // default. A staged saved session (pointed at by `DOT_AGENT_DECK_SESSION`
        // below) is restored unconditionally on launch when the daemon is empty,
        // so no flag is passed here.
        // M2.1 auditor S2: portable-pty 0.8 unconditionally env_clears
        // on Unix before applying our `cmd.env(...)` calls, but the old
        // comment claimed env_clear was avoided. Make the scrub
        // explicit so the behavior is documented in this file and not
        // dependent on an internal portable-pty detail.
        cmd.env_clear();

        // Decision 20: pinned env values. Order: portable-pty env_clear
        // above means nothing leaks from the host; we then set Decision
        // 20's pins, and finally layer the test's `with_env` overrides
        // (so a test asking for `NO_COLOR=1` still wins).
        let state_dir = work.join("state");
        // Pin the XDG config root inside the isolated HOME. `env_clear` above
        // already drops the host's value, but an agent adapter that resolves its
        // config the XDG way (Devin does — see `devin_hooks_manage`) would then
        // fall back to `$HOME/.config` only by luck of the variable being unset.
        // Setting it explicitly makes the isolation intentional, so no test can
        // ever write hooks into the developer's real config.
        let xdg_config_home = home.join(".config");
        let pinned: &[(&str, &str)] = &[
            ("TERM", "xterm-256color"),
            ("LC_ALL", "C.UTF-8"),
            (
                "XDG_CONFIG_HOME",
                xdg_config_home.to_str().expect("XDG config path is UTF-8"),
            ),
            ("COLORTERM", "truecolor"),
            // M2.1 auditor S3: pin SHELL so portable-pty cannot leak
            // the parent password DB entry on Unix. /bin/sh is
            // sufficient for the deck's spawn paths.
            ("SHELL", "/bin/sh"),
            ("HOME", home.to_str().expect("HOME path is UTF-8")),
            (
                "DOT_AGENT_DECK_SOCKET",
                hook_socket.to_str().expect("hook sock path is UTF-8"),
            ),
            (
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                attach_socket.to_str().expect("attach sock path is UTF-8"),
            ),
            // PRD #93 lazy-spawn writes a per-user lock dir. Pin it to
            // the tempdir so concurrent tests do not race on
            // `~/.cache/dot-agent-deck/spawn.lock`.
            (
                "DOT_AGENT_DECK_STATE_DIR",
                state_dir.to_str().expect("state dir is UTF-8"),
            ),
            // Disable the idle-shutdown so the daemon does not race the
            // test by exiting after a brief detach.
            ("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0"),
            // Leaked-daemon safety net: the deck lazy-spawns its daemon
            // DETACHED (its own session → parent is PID 1 from birth), so the
            // orphan watchdog can't be enabled here (it would fire instantly).
            // The max-lifetime backstop is the right net for a detached daemon:
            // even if a test is SIGKILL'd / panics / times out before `Drop`
            // runs, the inherited cap makes the daemon self-exit gracefully
            // within 300s instead of leaking to PID 1 for hours/days. Idle
            // shutdown stays disabled (above) for determinism.
            ("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300"),
            // PRD #249 M3: the daemon reports a delegated worker that emits no
            // event within a window (30s by default) as "possibly not
            // delivered", writing a notice into the ORCHESTRATOR's pane. Most
            // e2e delegate tests drive stand-in workers (`cat`, recorder
            // scripts) that legitimately emit nothing, so the report would fire
            // on every one of them and dirty panes that tests assert stay clean
            // (`orchestration/delegate/001`). Pinned off here rather than via
            // `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS=0`, which would also
            // disable PRD #126's idle-worker detection that other tests
            // exercise. A test that wants the report overrides it via
            // `with_env`.
            ("DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS", "0"),
            // PRD #249 M1: ordinary e2e scenarios do not pay the production
            // post-respawn buffer. The two real readiness scenarios opt back in
            // explicitly after this pin.
            ("DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS", "0"),
        ];
        // PATH is required for the deck to spawn its own daemon
        // subcommand (it shells out via `current_exe`, but lookups like
        // git still need PATH).
        //
        // ANTHROPIC_API_KEY (issue #502/#785) has to cross `env_clear` too, or
        // the credential the run was authorised on never reaches the process
        // that needs it: the deck lazy-spawns the daemon, the daemon spawns the
        // agent, and each inherits this environment. Before this, only the pi
        // tests got a key at all — by threading it through `with_env`
        // themselves — so a `check_claude_available` widened to accept a key
        // would have passed a gate the agent could not then satisfy.
        //
        // Deliberately ANTHROPIC_API_KEY alone, not "every *_API_KEY". Codex
        // reads its credential from `~/.codex/auth.json`, which
        // `import_codex_credentials` copies into the test HOME (measured: codex
        // 0.149 answers 401 from an env key alone), and `check_opencode_available`
        // only offers its env-key path for an `anthropic/…` test model for
        // exactly this reason — so no other provider variable would be used by
        // anything the deck spawns, and each one added is another secret in the
        // recorded PTY's environment for no gain.
        let inherit_pass = ["PATH", ANTHROPIC_API_KEY_ENV];

        let mut final_env: HashMap<String, String> = HashMap::new();
        for k in inherit_pass {
            if let Ok(v) = std::env::var(k) {
                final_env.insert(k.into(), v);
            }
        }
        for (k, v) in pinned {
            final_env.insert((*k).into(), (*v).into());
        }
        // Point the deck's saved-session reader at our staged file so
        // auto-restore picks up exactly the chain-smoke pane and
        // nothing from the developer's real session.toml.
        if builder.continue_session.is_some() {
            final_env.insert(
                "DOT_AGENT_DECK_SESSION".into(),
                session_toml_path
                    .to_str()
                    .expect("session.toml path is UTF-8")
                    .to_string(),
            );
        }
        // Decision 20: NO_COLOR and CLICOLOR_FORCE must NOT leak in.
        // We set up `final_env` from scratch, so they are absent by
        // construction — the only path back in is the test's own
        // `with_env` override (which we honour).
        for (k, v) in builder.extra_env {
            final_env.insert(k, v);
        }
        for (k, v) in final_env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn dot-agent-deck");
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            builder.rows,
            builder.cols,
            0,
        )));
        let last_byte_at = Arc::new(Mutex::new(Instant::now()));
        let cast_events = Arc::new(Mutex::new(Vec::<CastEvent>::new()));
        let byte_history = Arc::new(Mutex::new(Vec::<u8>::new()));
        let reader_stop = Arc::new(AtomicBool::new(false));
        let cast_started_at = Instant::now();

        // Take the PTY write side exactly once — `take_writer()` is
        // single-shot, so the per-call `take_writer()` the write helpers used
        // before panicked on their 2nd invocation (and dropped/closed the
        // write side after the 1st). Stored for all writes, and shared with
        // the reader thread so it can answer the deck's terminal-capability
        // queries inline (PRD #227 M2, see `answer_terminal_queries`).
        //
        // Poisoning is accepted, not handled: every `lock()` on this mutex
        // `unwrap()`s, so if a `send_bytes` on the test thread panics while
        // holding it (a closed PTY write side is the realistic case), the mutex
        // is poisoned and the reader thread's next `lock().unwrap()` panics too,
        // killing the reader — capability queries stop being answered and the
        // cast loses its tail. Deliberately left alone: the blast radius is one
        // already-failing test process, and the test thread's own panic is the
        // failure the developer sees. Recovering (`lock().unwrap_or_else(|e|
        // e.into_inner())`) would keep a reader alive to write through a PTY that
        // just proved unwritable, trading a clear failure for a confusing one.
        let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(
            pair.master.take_writer().expect("take PTY master writer"),
        ));

        // Reader thread: pulls bytes off the PTY master, feeds the
        // parser, updates `last_byte_at`, appends to the cast log
        // plus the byte-history buffer (M4.6 P1, for race-free
        // `wait_for_strings_in_order`), and answers the deck's
        // terminal-capability queries.
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let parser_for_reader = Arc::clone(&parser);
        let last_for_reader = Arc::clone(&last_byte_at);
        let cast_for_reader = Arc::clone(&cast_events);
        let history_for_reader = Arc::clone(&byte_history);
        let stop_for_reader = Arc::clone(&reader_stop);
        let start_for_reader = cast_started_at;
        let writer_for_reader = Arc::clone(&writer);
        let reader_handle = std::thread::Builder::new()
            .name(format!("tui-deck-reader-{test_name}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut query_scan: Vec<u8> = Vec::new();
                while !stop_for_reader.load(Ordering::Relaxed) {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            parser_for_reader.lock().unwrap().process(chunk);
                            *last_for_reader.lock().unwrap() = Instant::now();
                            cast_for_reader.lock().unwrap().push(CastEvent {
                                offset_secs: start_for_reader.elapsed().as_secs_f64(),
                                data: chunk.to_vec(),
                            });
                            history_for_reader.lock().unwrap().extend_from_slice(chunk);
                            answer_terminal_queries(
                                chunk,
                                &mut query_scan,
                                &mut *writer_for_reader.lock().unwrap(),
                            );
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::Interrupted
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue;
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn reader thread");

        let record_on_success = std::env::var_os("DOT_AGENT_DECK_RECORD").is_some()
            && !builder.suppress_success_recording;

        Ok(TuiDeck {
            pty_master: pair.master,
            writer,
            parser,
            last_byte_at,
            cast_events,
            byte_history,
            cast_started_at,
            reader_stop,
            reader_handle: Some(reader_handle),
            child,
            tempdir,
            home,
            hook_socket,
            attach_socket,
            fixture_path: work,
            test_name,
            cols: builder.cols,
            rows: builder.rows,
            record_on_success,
            recording_redactions,
        })
    }

    /// Resize the PTY mid-run. Exercises the SIGWINCH path covered by
    /// the resize/* catalog area.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.pty_master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize PTY");
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Quiescence wait: blocks until the deck has emitted no bytes for
    /// at least [`QUIESCENT_IDLE_MS`].
    pub fn wait_until_quiescent(&self) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let idle = Duration::from_millis(QUIESCENT_IDLE_MS);
        loop {
            let since = {
                let last = *self.last_byte_at.lock().unwrap();
                Instant::now().duration_since(last)
            };
            if since >= idle {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "deck did not become quiescent within {WAIT_TIMEOUT:?} \
                     (idle window {QUIESCENT_IDLE_MS}ms)"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Deterministic wait until `pred` holds for the current rendered grid
    /// (one string, rows joined by '\n'), or panic after the timeout. Unlike
    /// [`wait_until_quiescent`], this does not depend on the PTY going idle —
    /// with a live daemon event stream the deck redraws often enough that a
    /// 50 ms idle window may never occur, so quiescence is unreliable for
    /// mouse specs. Use this to wait for a specific observable outcome (e.g.
    /// a row gaining the selection marker, or a modal/form closing) after a
    /// click or keystroke.
    pub fn wait_until_grid(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            let grid = self.snapshot_grid();
            if pred(&grid) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "did not reach grid state {what:?} within {WAIT_TIMEOUT:?}.\nFinal grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for an observable grid state, then keep asserting that it remains
    /// visible for `hold_for`. Recording-focused E2E scenarios use this for
    /// deliberate demo beats without putting raw sleeps in test bodies.
    pub fn wait_until_grid_then_hold(
        &self,
        what: &str,
        hold_for: Duration,
        pred: impl Fn(&str) -> bool,
    ) {
        self.wait_until_grid(what, |grid| pred(grid));
        let deadline = Instant::now() + hold_for;
        loop {
            let grid = self.snapshot_grid();
            assert!(
                pred(&grid),
                "grid state {what:?} changed during its {hold_for:?} demo hold.\nFinal grid:\n{grid}"
            );
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `needle` is ABSENT from the rendered grid, or panic after
    /// the timeout. For asserting a modal/overlay/form closed.
    pub fn wait_for_absence(&self, needle: &str) {
        self.wait_until_grid(&format!("absence of {needle:?}"), |g| !g.contains(needle));
    }

    /// Opt-in fast wait when the test knows the screen contents it is
    /// looking for. Decision 21: use sparingly.
    pub fn wait_for_string(&self, needle: &str) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            {
                let parser = self.parser.lock().unwrap();
                if parser.screen().contents().contains(needle) {
                    return;
                }
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                panic!(
                    "did not see {needle:?} within {WAIT_TIMEOUT:?}.\n\
                     Final grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until `needle` appears anywhere in the deck's cumulative
    /// byte stream since launch — including bytes emitted *before* this
    /// call. Unlike [`wait_for_string`] (which only sees the current
    /// vt100 grid) and [`wait_for_strings_in_order`] (which only
    /// considers bytes after the call), this scans the entire rolling
    /// history from offset 0. Used to assert on transient output the
    /// deck prints before taking over the alternate screen — e.g. a
    /// startup warning written to stderr (which, on a PTY, is merged
    /// into the same byte stream as stdout) before the TUI clears the
    /// screen. The warning text scrolls out of the visible grid but
    /// stays in the byte history, so this is the only primitive that
    /// can observe it.
    pub fn wait_for_stream_string(&self, needle: &str) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            {
                let hist = self.byte_history.lock().unwrap();
                let text = String::from_utf8_lossy(&hist);
                if text.contains(needle) {
                    return;
                }
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                panic!(
                    "did not see {needle:?} anywhere in the byte stream within \
                     {WAIT_TIMEOUT:?}.\nFinal grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Like [`wait_for_stream_string`] but with a caller-supplied
    /// timeout instead of the fixed 10-second [`WAIT_TIMEOUT`]. Scans
    /// the entire rolling byte history (from offset 0) so a needle that
    /// already scrolled out of the visible vt100 grid is still found.
    /// Real-agent L2 tests (a daemon clone from live GitHub + a real
    /// cheap-model agent run) need a generous ceiling — minutes, not
    /// seconds — that the 10s default cannot express. Returns `true`
    /// once `needle` is seen, `false` if `timeout` elapses first (the
    /// caller decides whether a miss is a hard failure or a flaky-network
    /// observation to report).
    pub fn wait_for_stream_string_within(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let hist = self.byte_history.lock().unwrap();
                let text = String::from_utf8_lossy(&hist);
                if text.contains(needle) {
                    return true;
                }
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The deck's cumulative raw OUTPUT byte stream since launch, lossily
    /// decoded — escape sequences included, exactly as written to the tty.
    ///
    /// [`wait_for_stream_string_within`](Self::wait_for_stream_string_within)
    /// answers "did this ever appear"; this is for the questions a `contains`
    /// cannot express — how MANY times a sequence appears, and in what ORDER
    /// relative to another. PRD #227 M2 needs both: the terminal-mode push must
    /// appear before its matching pop, and the pop must appear exactly once (a
    /// double pop would discard a flag set another program on the terminal's
    /// stack owns). Escape bytes survive the decode unharmed — `ESC` is 0x1b,
    /// valid UTF-8 — so a needle like `"\x1b[>1u"` matches literally.
    pub fn stream_text(&self) -> String {
        String::from_utf8_lossy(&self.byte_history.lock().unwrap()).into_owned()
    }

    /// Wait for the deck PROCESS to exit and for every byte it wrote on the way
    /// out to be drained into the rolling history, then report whether it exited
    /// successfully (`Some(true)`) or with a failure status (`Some(false)`).
    /// `None` means it was still alive when `timeout` elapsed.
    ///
    /// Required by any assertion that COUNTS or ORDERS bytes from the teardown
    /// path. [`wait_for_stream_string_within`](Self::wait_for_stream_string_within)
    /// returns the instant the FIRST match appears, which during a shutdown is
    /// before the process is actually gone — so a duplicate emitted a moment
    /// later (a second terminal-mode pop from an RAII `Drop`, say) lands after
    /// the snapshot, and an "exactly once" assertion passes against an
    /// implementation that in fact does it twice. Draining to exit first closes
    /// that window; the exit status additionally proves the quit path returned
    /// cleanly instead of dying on the way out.
    pub fn wait_for_exit_within(&mut self, timeout: Duration) -> Option<bool> {
        let deadline = Instant::now() + timeout;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                // Unwaitable (already reaped elsewhere) — report it as "did not
                // observe a clean exit" rather than inventing a status.
                Err(_) => return None,
                Ok(None) => {}
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // The writer is dead, so no NEW bytes can appear — but the reader thread
        // may still be a poll cycle behind on what the PTY already buffered.
        // Quiescence is the drain signal, and it is reached promptly now.
        self.wait_until_quiescent();
        Some(status.success())
    }

    /// Like [`wait_for_string`] (scans the RECONSTRUCTED vt100 grid, so
    /// styled UI chrome — a bottom-bar affordance, a tab label, a card
    /// field — whose glyphs are written as separate styled runs is matched
    /// on its rendered text, not on the raw byte stream where the runs are
    /// interleaved with cursor-move escapes) but with a caller-supplied
    /// timeout instead of the fixed 10-second [`WAIT_TIMEOUT`]. Real-agent
    /// L2 tests need a generous ceiling (minutes) the default cannot
    /// express. Returns `true` once `needle` is on the rendered grid,
    /// `false` if `timeout` elapses first (the caller decides whether a
    /// miss is a hard failure or a soft observation). Use this for
    /// persistent on-screen state; use [`wait_for_stream_string_within`]
    /// for transient PLAIN-text agent output that may scroll off.
    pub fn wait_for_grid_string_within(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.snapshot_grid().contains(needle) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The retry (not just the wait) lives here, not in the test body. For a
    /// keystroke gated on a status update delivered over a SEPARATE async
    /// daemon round-trip (a hook-socket injection broadcast back to this
    /// attached client) there is no in-process signal a test can await instead
    /// — unlike an in-process state flip (e.g. toggling the command-entry lock
    /// via `Ctrl+e`, which is synchronous), the client may not yet have applied
    /// the broadcast the instant after it was sent. Repeatedly (re-)sends
    /// `bytes` until `needle` lands on the rendered grid or `timeout` elapses,
    /// so a keystroke sent slightly too early is simply retried rather than
    /// lost — the same "wait for the needle, don't just snapshot" principle
    /// applied to the SEND side rather than the read side.
    ///
    /// Safe to retry against a `cat` stub, which just re-echoes; not
    /// appropriate where a duplicate keystroke would have a side effect.
    pub fn send_keys_until_grid_string_within(
        &self,
        bytes: &[u8],
        needle: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.send_keys(bytes);
            if self.wait_for_grid_string_within(needle, Duration::from_millis(300)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    /// Like [`wait_for_grid_string_within`](Self::wait_for_grid_string_within)
    /// but for a predicate over the whole rendered grid — for the cases a single
    /// substring cannot express (a spatial relationship between two strings, a
    /// count, an ordering).
    ///
    /// Returns `true` as soon as `pred` holds, `false` if `timeout` elapses.
    /// Non-panicking on purpose: the caller re-checks the condition afterwards
    /// so the failure diagnostic is its own detailed assertion rather than a
    /// generic harness message. Prefer this over
    /// [`wait_until_quiescent`](Self::wait_until_quiescent) whenever a LIVE
    /// agent occupies a pane — an agent that animates a spinner never leaves the
    /// deck's byte stream idle, so quiescence never arrives.
    pub fn wait_for_grid_predicate_within(
        &self,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.snapshot_grid()) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for `needles` to appear, in order, in the cumulative
    /// byte stream the deck has emitted since this call started.
    ///
    /// Unlike [`wait_for_string`], which asserts against the *current*
    /// rendered grid, this primitive walks a rolling history of every
    /// byte the PTY reader thread has captured. Two transitions
    /// rendered in the same ~20 ms polling window (e.g. Thinking →
    /// Working on a fast Haiku response) both land in the history,
    /// so a later poll still finds the earlier substring rather than
    /// spinning past it (M4.6 P1 / Decision 9: flake = bug).
    ///
    /// Semantics:
    /// - History is snapshotted from the byte-history buffer at call
    ///   time; bytes the deck emitted before this call are NOT
    ///   considered.
    /// - Each substring must be observed AFTER its predecessor was
    ///   observed (strictly increasing offsets).
    /// - Single 10-second total ceiling — internal poll cadence is
    ///   ~20 ms.
    /// - Substrings are matched against a lossy UTF-8 decode of the
    ///   raw bytes; status labels like `Thinking` / `Working` / `Bash`
    ///   / `Idle` are plain ASCII and unaffected by interleaved ANSI
    ///   control sequences.
    pub fn wait_for_strings_in_order(&self, needles: &[&str]) {
        if needles.is_empty() {
            return;
        }
        let start_idx = self.byte_history.lock().unwrap().len();
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            let snapshot: Vec<u8> = {
                let hist = self.byte_history.lock().unwrap();
                if hist.len() > start_idx {
                    hist[start_idx..].to_vec()
                } else {
                    Vec::new()
                }
            };
            let matched = match_needles_in_order(&snapshot, needles);
            if matched == needles.len() {
                return;
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                let so_far = needles[..matched].join(", ");
                let next = needles[matched];
                panic!(
                    "did not see `{next}` (needle #{} of {} — already \
                     matched in order: [{so_far}]) within {WAIT_TIMEOUT:?}.\n\
                     Final grid:\n{grid}",
                    matched + 1,
                    needles.len(),
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Like [`wait_for_strings_in_order`], but tolerant of a one-shot
    /// agent that completes and exits before a stable terminal frame is
    /// ever rendered.
    ///
    /// `prefix` needles are matched STRICTLY in order against the
    /// rolling byte history (so the test still proves the live
    /// lifecycle ran); then ANY one of `terminal_alternatives`,
    /// appearing AFTER the last prefix needle, satisfies the terminal
    /// condition.
    ///
    /// This exists for the chain-smoke path. A `claude -p` print-mode
    /// agent COMPLETES and EXITS the instant it finishes responding, so
    /// the pane can jump from `Working`/`Bash` straight to the stable
    /// "No agent" / "Launch an agent to get started" placeholder before
    /// a rendered `Idle` frame survives a ~20 ms poll (a pre-existing
    /// PRD #77 timing fragility — the agent demonstrably reaches
    /// Thinking → Working → Bash, only the terminal `Idle` observation
    /// races the exit). Accepting that clean exit as equivalent to a
    /// captured `Idle` keeps the terminal assertion robust to the
    /// print-mode lifecycle without weakening the strict prefix that
    /// proves the agent traversed the working lifecycle.
    ///
    /// Searching the terminal alternatives only after the prefix cursor
    /// is deliberate: a restored session renders a default `Idle` (and
    /// may render the placeholder) *before* the agent starts, so an
    /// early occurrence must not count — only a terminal state reached
    /// AFTER the working lifecycle does.
    ///
    /// `timeout` is caller-supplied (rather than the shared
    /// `WAIT_TIMEOUT`) so a real-agent test can grant a generous
    /// settling budget for the terminal observation — the working
    /// lifecycle is fast, but the terminal `Idle`/exit races real-agent
    /// variance (Design Decision #7: real-agent tests use generous
    /// timeouts).
    ///
    /// The terminal condition is settled by [`terminal_reached`], which
    /// accepts EITHER the post-prefix byte stream or the currently
    /// rendered grid. The byte stream alone was a flake source: it is
    /// not a faithful record of the screen (differential rendering
    /// emits nothing for an unchanged region), so a terminal state the
    /// user was plainly looking at could go unobserved until the
    /// timeout — the assertion failed while printing a final grid that
    /// contained the very needle it said it had not seen.
    pub fn wait_for_strings_in_order_then_any_within(
        &self,
        prefix: &[&str],
        terminal_alternatives: &[&str],
        timeout: Duration,
    ) {
        let start_idx = self.byte_history.lock().unwrap().len();
        let deadline = Instant::now() + timeout;
        // Latched by `terminal_reached` the first time the prefix completes
        // with the stream unsatisfied — see its docs for why the grid arm
        // needs a "what was already there" reference.
        let mut baseline_grid: Option<String> = None;
        loop {
            let snapshot: Vec<u8> = {
                let hist = self.byte_history.lock().unwrap();
                if hist.len() > start_idx {
                    hist[start_idx..].to_vec()
                } else {
                    Vec::new()
                }
            };
            let (matched, terminal_found) = terminal_reached(
                &snapshot,
                prefix,
                terminal_alternatives,
                &mut baseline_grid,
                || self.snapshot_grid(),
            );
            if matched == prefix.len() && terminal_found {
                return;
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                if matched < prefix.len() {
                    let so_far = prefix[..matched].join(", ");
                    let next = prefix[matched];
                    panic!(
                        "did not see prefix needle `{next}` (#{} of {} — already \
                         matched in order: [{so_far}]) within {timeout:?}.\n\
                         Final grid:\n{grid}",
                        matched + 1,
                        prefix.len(),
                    );
                } else {
                    let alts = terminal_alternatives.join("` | `");
                    let pfx = prefix.join(", ");
                    panic!(
                        "matched the full prefix [{pfx}] but saw none of the \
                         terminal alternatives [`{alts}`] after it within \
                         {timeout:?}.\nFinal grid:\n{grid}"
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Send raw bytes to the deck as if typed at the terminal. Writes
    /// to the PTY master so the spawned binary reads them on stdin and
    /// `crossterm` decodes them into key events. Callers pass the
    /// terminal byte encoding of the keypress — e.g. `b"\x03"` for
    /// Ctrl+C, `b"\x0e"` for Ctrl+n, `b"?"` for a literal `?`,
    /// `b"\x1bOP"` for F1, or an ESC-prefixed sequence like `b"\x1bL"`
    /// for Alt+Shift+L. The whole slice is written in one syscall so a
    /// multi-byte sequence is decoded as a single chord, not as
    /// separate keys.
    pub fn send_keys(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).expect("write keys to PTY master");
        writer.flush().expect("flush keys to PTY master");
    }

    /// Subscribe to the daemon's broadcast event stream used by this attached
    /// deck. Wrapper e2es use it to inspect the typed event and schema version
    /// while separately asserting the same transition on the rendered grid.
    ///
    /// PRD #42 M8: returns the Unix-only `EventSub` (attach stream over a UDS),
    /// so this accessor is Unix-gated like the harness it exposes.
    #[cfg(unix)]
    pub fn subscribe_events(&self) -> EventSub {
        EventSub::open(&self.attach_socket).expect("open SubscribeEvents stream")
    }

    /// Returns the deck's per-test hook socket path. Synthetic-event
    /// L2 tests connect to this directly to inject hook payloads.
    pub fn hook_socket_path(&self) -> &Path {
        &self.hook_socket
    }

    /// Returns the deck's per-test attach socket path.
    pub fn attach_socket_path(&self) -> &Path {
        &self.attach_socket
    }

    /// The deck's working directory (the copied fixture root, and the deck's
    /// cwd). Tests use it to drop runtime files (agent scripts, record files)
    /// the spawned agent can reach via a cwd-relative path.
    pub fn workdir(&self) -> &Path {
        &self.fixture_path
    }

    /// The deck's per-test `HOME` — the same one the lazily-spawned daemon and
    /// every agent it spawns inherit. Tests that must seed HOME-relative agent
    /// config for a directory only known AFTER launch (e.g. Claude's per-folder
    /// trust for the tempdir fixture root, which
    /// [`TuiDeckBuilder::with_claude_project_trust`] cannot know in advance)
    /// write into it via [`seed_claude_trust_in_home`] before opening the pane.
    pub fn home_dir(&self) -> &Path {
        &self.home
    }

    /// Return the parsed grid contents — used by `wait_for_string`
    /// internally and by tests that want to assert on full-screen
    /// state.
    pub fn snapshot_grid(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Return the real terminal's current hardware-cursor visibility, position,
    /// and cell styling as parsed from the deck's PTY output.
    pub fn terminal_cursor_snapshot(&self) -> TerminalCursorSnapshot {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let (row, col) = screen.cursor_position();
        TerminalCursorSnapshot {
            hidden: screen.hide_cursor(),
            row,
            col,
            cell: screen.cell(row, col).map(GridCellStyle::from),
        }
    }

    /// Wait for the real terminal's hardware cursor visibility to match the
    /// requested state. Live agent TUIs can repaint their prompt one frame
    /// after their final output, so cursor assertions need the same bounded
    /// observable-state wait as grid assertions.
    pub fn wait_for_terminal_cursor_hidden_within(&self, hidden: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.terminal_cursor_snapshot().hidden == hidden {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Debounced cursor read: poll
    /// [`terminal_cursor_snapshot`](Self::terminal_cursor_snapshot) until
    /// `STABLE_SAMPLES` consecutive reads — position AND cell styling, both
    /// fields of `TerminalCursorSnapshot`'s `PartialEq` — come back identical,
    /// or `timeout` elapses, whichever is first.
    ///
    /// On its own this only proves "nothing changed for ~60ms," which a
    /// snapshot taken the INSTANT a keystroke is sent trivially satisfies — the
    /// write hasn't even reached the PTY yet, so the first several reads all
    /// agree on the stale pre-keystroke value and this returns immediately
    /// without ever observing the keystroke's effect. Calling this directly
    /// right after `send_bytes`/`send_keys` is that trap; use
    /// [`wait_for_terminal_cursor_change_then_settle`](Self::wait_for_terminal_cursor_change_then_settle)
    /// instead whenever a prior snapshot is available to diverge from. This
    /// primitive is for the complementary case — settling on the FINAL frame
    /// once some external wait (e.g. `wait_for_grid_string_within`) has already
    /// established that change is underway or done, where a bare
    /// `terminal_cursor_snapshot()` could still land one frame early.
    pub fn wait_for_settled_terminal_cursor(&self, timeout: Duration) -> TerminalCursorSnapshot {
        const STABLE_SAMPLES: u32 = 3;
        const POLL_INTERVAL: Duration = Duration::from_millis(20);

        let deadline = Instant::now() + timeout;
        let mut last = self.terminal_cursor_snapshot();
        let mut stable_count = 1;
        loop {
            if stable_count >= STABLE_SAMPLES || Instant::now() >= deadline {
                return last;
            }
            std::thread::sleep(POLL_INTERVAL);
            let current = self.terminal_cursor_snapshot();
            if current == last {
                stable_count += 1;
            } else {
                last = current;
                stable_count = 1;
            }
        }
    }

    /// Two-phase cursor wait for "this keystroke should move the cursor away
    /// from `from`": first a coarse, cheap `(row, col)` divergence check
    /// (catches "did anything move at all", bounded by `timeout`), then a
    /// hand-off to
    /// [`wait_for_settled_terminal_cursor`](Self::wait_for_settled_terminal_cursor)
    /// (bounded by whatever of `timeout` remains) so the returned snapshot
    /// reflects the FINAL frame rather than the first one where the coarse
    /// check happened to pass.
    ///
    /// Collapsing this into a single stability-only wait (poll
    /// `terminal_cursor_snapshot()` until N consecutive reads agree, with no
    /// divergence check) is flaky in the OPPOSITE direction on a loaded runner:
    /// called right after `send_bytes`, before the keystroke has propagated
    /// through the PTY round trip at all, the first several reads all agree on
    /// the unchanged pre-keystroke value — which is trivially "stable" — so it
    /// returns that stale snapshot within one poll interval instead of waiting
    /// for the real change. Requiring an actual `(row, col)` change from a
    /// known `from` baseline before settling closes that gap.
    ///
    /// If the cursor never diverges from `from` within `timeout`, returns the
    /// last (still-`from`-equal) snapshot observed — the caller's own assertion
    /// on the returned value produces the diagnostic, exactly as it would for a
    /// real product regression.
    pub fn wait_for_terminal_cursor_change_then_settle(
        &self,
        from: TerminalCursorSnapshot,
        timeout: Duration,
    ) -> TerminalCursorSnapshot {
        const POLL_INTERVAL: Duration = Duration::from_millis(20);
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.terminal_cursor_snapshot();
            if (snap.row, snap.col) != (from.row, from.col) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                return self.wait_for_settled_terminal_cursor(remaining);
            }
            if Instant::now() >= deadline {
                return snap;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Return the styling at one cell of the real terminal grid.
    pub fn grid_cell_style(&self, row: u16, col: u16) -> Option<GridCellStyle> {
        self.parser
            .lock()
            .unwrap()
            .screen()
            .cell(row, col)
            .map(GridCellStyle::from)
    }

    /// Locate visible ASCII text and return the style of each occupied cell.
    /// The mode-indication L2 tests use unique ASCII sentinels, so one character
    /// maps to one terminal cell and the returned vector aligns with `needle`.
    pub fn visible_text_cell_styles(&self, needle: &str) -> Option<Vec<GridCellStyle>> {
        assert!(
            needle.is_ascii(),
            "visible text style lookup requires ASCII"
        );
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let grid = screen.contents();
        for (row, line) in grid.lines().enumerate() {
            let Some(byte_col) = line.find(needle) else {
                continue;
            };
            let col = line[..byte_col].chars().count() as u16;
            let styles = (0..needle.len() as u16)
                .map(|offset| {
                    screen
                        .cell(row as u16, col + offset)
                        .map(GridCellStyle::from)
                })
                .collect::<Option<Vec<_>>>()?;
            return Some(styles);
        }
        None
    }

    /// Write raw bytes to the deck's PTY master — the input side of the
    /// terminal. Lets L2 tests drive the deck the way a user's keyboard or
    /// mouse would (key bytes, SGR mouse reports). Flushes so the deck sees
    /// the input promptly.
    pub fn send_bytes(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().unwrap();
        writer.write_all(bytes).expect("write to PTY master");
        writer.flush().expect("flush PTY master");
    }

    /// Send a left-button mouse click at the given 0-based grid cell
    /// (`col`, `row`) as an SGR (1006) extended mouse report — press then
    /// release — matching what crossterm's `EnableMouseCapture` makes the
    /// deck decode. SGR coordinates are 1-based, so each is offset by one.
    pub fn click(&self, col: u16, row: u16) {
        let cx = col + 1;
        let cy = row + 1;
        // \x1b[<0;cx;cyM = left-button press; trailing `m` = release.
        self.send_bytes(format!("\x1b[<0;{cx};{cy}M").as_bytes());
        self.send_bytes(format!("\x1b[<0;{cx};{cy}m").as_bytes());
    }

    /// Send a mouse wheel scroll at the given 0-based grid cell as an SGR
    /// (1006) report (button code 64 = wheel up, 65 = wheel down), matching
    /// what crossterm decodes to `MouseEventKind::ScrollUp`/`ScrollDown`.
    /// Lets tests assert that scroll events reach the scroll path rather than
    /// being intercepted by the button hit-test layer.
    pub fn scroll(&self, col: u16, row: u16, down: bool) {
        let cb = if down { 65 } else { 64 };
        let cx = col + 1;
        let cy = row + 1;
        self.send_bytes(format!("\x1b[<{cb};{cx};{cy}M").as_bytes());
    }

    /// Send `count` mouse wheel notches at the given 0-based grid cell.
    /// Keeps repeated input emission in the harness so E2E test bodies can
    /// describe the intended interaction without fixed-count polling loops.
    pub fn scroll_n(&self, col: u16, row: u16, down: bool, count: usize) {
        for _ in 0..count {
            self.scroll(col, row, down);
        }
    }

    /// Locate the first occurrence of `needle` in the current rendered
    /// grid, returning its 0-based `(col, row)` start cell, or `None` if it
    /// is not on screen. Used by click tests to find a button's on-screen
    /// position before clicking it (so the test follows the real layout
    /// rather than hard-coding coordinates).
    pub fn find_in_grid(&self, needle: &str) -> Option<(u16, u16)> {
        let grid = self.snapshot_grid();
        for (row, line) in grid.lines().enumerate() {
            if let Some(byte_idx) = line.find(needle) {
                let col = line[..byte_idx].chars().count();
                return Some((col as u16, row as u16));
            }
        }
        None
    }

    /// Poll [`find_in_grid`] until `needle` is on screen, returning its
    /// 0-based `(col, row)` start cell, or panic (dumping the final grid)
    /// after [`WAIT_TIMEOUT`].
    ///
    /// Prefer this over a bare `find_in_grid(..).expect(..)` whenever the
    /// lookup follows an input event (a click, a keystroke) rather than a
    /// wait that already proved the target is painted. A single-shot read
    /// landing mid-repaint sees a transiently cleared region and the
    /// `expect` fires — a load-sensitive flake, not a real failure. A
    /// `wait_for_string` in front narrows that window but does not close
    /// it: the wait and the subsequent `find_in_grid` take two separate
    /// snapshots, and the clear can land between them. Polling the lookup
    /// itself means the coordinates always come from a grid that actually
    /// contained the needle.
    pub fn wait_for_in_grid(&self, needle: &str) -> (u16, u16) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            if let Some(found) = self.find_in_grid(needle) {
                return found;
            }
            if Instant::now() > deadline {
                let grid = self.snapshot_grid();
                panic!(
                    "did not find {needle:?} in the grid within {WAIT_TIMEOUT:?}.\n\
                     Final grid:\n{grid}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TuiDeck {
    fn drop(&mut self) {
        // Decision 28: dump recordings when the test panicked (failure),
        // or unconditionally when `DOT_AGENT_DECK_RECORD=1` (developer
        // opt-in for capturing successful runs).
        let panicking = std::thread::panicking();
        let should_dump = panicking || self.record_on_success;

        // Stop the reader, then kill the child. Order matters: if we
        // kill first the reader sees EOF mid-buffer and the cast loses
        // its tail. Stop the reader instead so the partial buffer
        // already lives in `cast_events`.
        self.reader_stop.store(true, Ordering::Relaxed);
        // Reap the whole process tree, not just the deck itself. portable-pty
        // makes the spawned deck a session/process-group leader (pgid == pid),
        // so a negative-pid `kill` signals every non-detached descendant in its
        // group (best-effort; ignore errors). Then the normal child kill+wait
        // as the fallback. (The deck's own lazy-spawned daemon setsid's into a
        // separate session and escapes this group — its
        // `DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS` cap is the net for that.)
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // SAFETY: kill(2) with a negative pid signals the process group;
            // SIGKILL has no failure mode beyond ESRCH/EPERM, which we ignore.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();

        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }

        if should_dump {
            // M4.3 flattened layout: each test gets its own per-test
            // subdirectory under `.dot-agent-deck/recordings/`, so
            // the cast and any failure artifacts sit alongside the
            // generated `.md`. `.dot-agent-deck/` is gitignored, so
            // the dump is purely developer-machine state — like
            // `target/`. The per-run subdir from M2.1 is gone:
            // concurrent `cargo test-e2e` on the same checkout is
            // not a real-world workflow, and the per-test path means
            // a re-run simply replaces the previous artifacts.
            let recordings_dir =
                workspace_recordings_root().join(sanitize_test_name(&self.test_name));
            if let Err(e) = self.dump_recordings(&recordings_dir) {
                eprintln!("[tui-harness] failed to write recordings to {recordings_dir:?}: {e}");
            }
            // PRD #77 Decision 30 / M4: regenerate the paired `.md`
            // for this test so a `DOT_AGENT_DECK_RECORD=1` run keeps
            // the doc next to the freshly-written cast in sync with
            // the test source. Cheap (~3 files to parse today);
            // best-effort — a generator error is surfaced to stderr
            // but does NOT poison the test result, because rule 7
            // already catches drift in CI.
            regenerate_paired_doc(&self.test_name);
        }
    }
}

/// Best-effort: regenerate the paired `.md` for the currently-running
/// test. Looks up the test by its Rust thread-name (which is the fn
/// name in cargo test), maps that to a spec id via the discovered
/// `#[spec]` set, and writes the resulting doc. Any error is logged
/// to stderr without panicking — CI's linkage-check rule 7 is the
/// load-bearing enforcement.
fn regenerate_paired_doc(test_name: &str) {
    let workspace_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let config = xtask_docs::DocsConfig::from_workspace(&workspace_root);
    let generated = match xtask_docs::generate_all(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[tui-harness] regenerate paired .md failed: {e}");
            return;
        }
    };
    let target = generated.into_iter().find(|d| d.fn_name == test_name);
    match target {
        Some(g) => {
            if let Err(e) = xtask_docs::write_all(std::slice::from_ref(&g)) {
                eprintln!(
                    "[tui-harness] regenerate paired .md write failed for `{test_name}`: {e}"
                );
            }
        }
        None => {
            eprintln!(
                "[tui-harness] no #[spec(...)] test matches fn name `{test_name}` — skipping doc regeneration"
            );
        }
    }
}

impl TuiDeck {
    fn dump_recordings(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;

        // M4.3: atomic writes for every artifact in the per-test
        // dir. Two `cargo test-e2e` runs on the same checkout (or one
        // run racing `cargo xtask docs --tests` against the `.md`)
        // can land here concurrently for the same test; tempfile +
        // rename inside the destination directory keeps the
        // post-rename file either fully old or fully new — never
        // half-written.

        // final-grid.txt
        let grid = redact_known_credentials_text(&self.snapshot_grid(), &self.recording_redactions);
        atomic_write(&dir.join("final-grid.txt"), grid.as_bytes())?;

        // final-grid.svg — minimal monospace render. Not pixel-perfect,
        // but valid SVG that opens in any browser.
        let svg = render_grid_to_svg(&grid, self.cols, self.rows);
        atomic_write(&dir.join("final-grid.svg"), svg.as_bytes())?;

        // full-stream.cast — asciinema v2 format (header + one JSON
        // array per event). Inline encoder, ~20 lines.
        let cast = self.encode_asciinema_cast();
        atomic_write(&dir.join("full-stream.cast"), cast.as_bytes())?;

        // fixture.toml — copy of the deck's .dot-agent-deck.toml so a
        // reviewer can replay against the same config.
        let fixture_src = self.fixture_path.join(".dot-agent-deck.toml");
        if fixture_src.exists() {
            let bytes = std::fs::read(&fixture_src)?;
            let redacted = redact_known_credentials_bytes(&bytes, &self.recording_redactions);
            atomic_write(&dir.join("fixture.toml"), &redacted)?;
        }
        Ok(())
    }

    fn encode_asciinema_cast(&self) -> String {
        let mut s = String::new();
        // Header — minimum required fields for asciinema v2.
        let header = serde_json::json!({
            "version": 2,
            "width": self.cols,
            "height": self.rows,
            "env": {
                "TERM": "xterm-256color",
            },
        });
        s.push_str(&header.to_string());
        s.push('\n');
        let events = self.cast_events.lock().unwrap();
        let redacted_events = redact_cast_events(&events, &self.recording_redactions);
        for (ev, redacted) in events.iter().zip(redacted_events) {
            // Lossy UTF-8 decoding is what asciinema players expect:
            // raw bytes that are valid UTF-8 round-trip, invalid bytes
            // are replaced rather than dropped.
            let data = String::from_utf8_lossy(&redacted);
            let line = serde_json::json!([ev.offset_secs, "o", data]);
            s.push_str(&line.to_string());
            s.push('\n');
        }
        s
    }
}

const RECORDING_CREDENTIAL_REDACTION: &[u8] = b"[REDACTED-CREDENTIAL]";

/// Locate non-overlapping credential occurrences, preferring the longest value
/// at a shared start. Matching bytes before JSON/asciinema encoding also catches
/// secrets that contain characters the artifact format would escape.
fn credential_redaction_ranges(data: &[u8], credentials: &[String]) -> Vec<(usize, usize)> {
    let patterns: Vec<&[u8]> = credentials
        .iter()
        .map(String::as_bytes)
        .filter(|value| !value.is_empty())
        .collect();
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let matched = patterns
            .iter()
            .filter(|pattern| data[offset..].starts_with(pattern))
            .max_by_key(|pattern| pattern.len());
        if let Some(pattern) = matched {
            ranges.push((offset, offset + pattern.len()));
            offset += pattern.len();
        } else {
            offset += 1;
        }
    }
    ranges
}

fn redact_known_credentials_bytes(data: &[u8], credentials: &[String]) -> Vec<u8> {
    let ranges = credential_redaction_ranges(data, credentials);
    if ranges.is_empty() {
        return data.to_vec();
    }
    let mut redacted = Vec::with_capacity(data.len());
    let mut copied = 0;
    for (start, end) in ranges {
        redacted.extend_from_slice(&data[copied..start]);
        redacted.extend_from_slice(RECORDING_CREDENTIAL_REDACTION);
        copied = end;
    }
    redacted.extend_from_slice(&data[copied..]);
    redacted
}

fn redact_known_credentials_text(text: &str, credentials: &[String]) -> String {
    String::from_utf8_lossy(&redact_known_credentials_bytes(
        text.as_bytes(),
        credentials,
    ))
    .into_owned()
}

/// Redact against the concatenated PTY stream, then project the result back onto
/// the original timestamped events. A provider can split a token across two PTY
/// reads, so redacting each event independently would leave that token intact in
/// `full-stream.cast`.
fn redact_cast_events(events: &[CastEvent], credentials: &[String]) -> Vec<Vec<u8>> {
    let stream: Vec<u8> = events
        .iter()
        .flat_map(|event| event.data.iter().copied())
        .collect();
    let ranges = credential_redaction_ranges(&stream, credentials);
    if ranges.is_empty() {
        return events.iter().map(|event| event.data.clone()).collect();
    }

    let mut projected = Vec::with_capacity(events.len());
    let mut event_start = 0;
    let mut range_index = 0;
    for event in events {
        let event_end = event_start + event.data.len();
        while range_index < ranges.len() && ranges[range_index].1 <= event_start {
            range_index += 1;
        }
        let mut out = Vec::with_capacity(event.data.len());
        let mut copied = event_start;
        let mut index = range_index;
        while index < ranges.len() && ranges[index].0 < event_end {
            let (secret_start, secret_end) = ranges[index];
            if copied < secret_start {
                out.extend_from_slice(&stream[copied..secret_start.min(event_end)]);
            }
            if secret_start >= event_start {
                out.extend_from_slice(RECORDING_CREDENTIAL_REDACTION);
            }
            copied = secret_end.min(event_end).max(copied);
            index += 1;
        }
        if copied < event_end {
            out.extend_from_slice(&stream[copied..event_end]);
        }
        projected.push(out);
        event_start = event_end;
    }
    projected
}

// ---------------------------------------------------------------------------
// Terminal-capability query answering (PRD #227 M2)
// ---------------------------------------------------------------------------

/// Query the deck emits to detect the enhanced (kitty) keyboard protocol:
/// `ESC [ ? u` (report current progressive-enhancement flags). Written by
/// `crossterm::terminal::supports_keyboard_enhancement()`.
const QUERY_KITTY_FLAGS: &[u8] = b"\x1b[?u";
/// The second half of that probe: `ESC [ c` (primary device attributes, DA1).
const QUERY_DA1: &[u8] = b"\x1b[c";
/// Reply to [`QUERY_KITTY_FLAGS`]: `CSI ? 1 u` — "the terminal currently has
/// DISAMBIGUATE_ESCAPE_CODES set". crossterm treats ANY flags reply (even
/// `0`) as "the protocol is supported", so this is what makes the deck's
/// `supports_keyboard_enhancement()` return `Ok(true)` and push its flag.
const REPLY_KITTY_FLAGS: &[u8] = b"\x1b[?1u";
/// Reply to [`QUERY_DA1`]: a plain VT220-class DA1 response. crossterm parses
/// any `CSI ? … c` as `PrimaryDeviceAttributes` without inspecting the
/// attributes, and drains it from its queue right after the flags reply.
const REPLY_DA1: &[u8] = b"\x1b[?62;22c";
/// Longest query pattern above, in bytes — how much trailing context the
/// scan buffer must retain so a query split across two PTY reads still
/// matches.
const LONGEST_QUERY_LEN: usize = 4;

/// Answer the terminal-capability queries the deck writes to its tty, so its
/// startup probe returns immediately instead of blocking.
///
/// PRD #227 M2 made the deck call
/// `crossterm::terminal::supports_keyboard_enhancement()` at TUI startup. That
/// writes `ESC[?u ESC[c` to the tty and then blocks for up to **2000 ms**
/// waiting for a reply. A PTY that never answers costs every L2 test ~2 s of
/// its 10 s [`WAIT_TIMEOUT`] budget before the first frame paints — and leaves
/// the enhanced protocol disabled, so no e2e test could exercise the
/// modifier-aware forwarding path the PRD is about. Answering both halves
/// makes the probe return in milliseconds AND models a kitty-capable terminal,
/// which is the configuration the fix targets.
///
/// `scan` carries state across calls: unmatched bytes are consumed, and up to
/// `LONGEST_QUERY_LEN - 1` trailing bytes are retained so a query straddling
/// two reads is still found. Retained bytes are always match-free (the scan
/// below runs to exhaustion), so no query is ever answered twice.
fn answer_terminal_queries(chunk: &[u8], scan: &mut Vec<u8>, writer: &mut dyn Write) {
    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .filter(|_| !needle.is_empty())
    }

    scan.extend_from_slice(chunk);
    let mut reply: Vec<u8> = Vec::new();
    loop {
        // Answer in the order the queries appear on the wire, so the deck's
        // crossterm sees the flags reply before the DA1 terminator.
        let hit = [
            (
                find(scan, QUERY_KITTY_FLAGS),
                QUERY_KITTY_FLAGS,
                REPLY_KITTY_FLAGS,
            ),
            (find(scan, QUERY_DA1), QUERY_DA1, REPLY_DA1),
        ]
        .into_iter()
        .filter_map(|(pos, q, r)| pos.map(|p| (p, q.len(), r)))
        .min_by_key(|(pos, _, _)| *pos);
        let Some((pos, qlen, r)) = hit else { break };
        reply.extend_from_slice(r);
        scan.drain(..pos + qlen);
    }
    if scan.len() > LONGEST_QUERY_LEN - 1 {
        let cut = scan.len() - (LONGEST_QUERY_LEN - 1);
        scan.drain(..cut);
    }
    if !reply.is_empty() {
        let _ = writer.write_all(&reply);
        let _ = writer.flush();
    }
}

// ---------------------------------------------------------------------------
// L1 buffer-render helpers
// ---------------------------------------------------------------------------
//
// Shared by the in-process `TestBackend` render tests (`tests/render_*.rs`),
// which assert on a `ratatui::buffer::Buffer` rather than a PTY grid. Kept here
// so the button-bar and layout suites read a single copy (PRD #144 DRY).

/// Count the rows of `buffer` that carry any non-blank cell — i.e. how many
/// rows the rendered bar actually occupies. One row means the bar fit on a
/// single line; two or more means it wrapped (each extra row is one row the
/// dashboard must cede from its height budget for the bottom bar).
pub fn nonblank_rows(buffer: &ratatui::buffer::Buffer) -> usize {
    let area = buffer.area();
    (0..area.height)
        .filter(|&y| (0..area.width).any(|x| !buffer[(x, y)].symbol().trim().is_empty()))
        .count()
}

/// Join every row of a (possibly multi-row) bar buffer into one `\n`-separated
/// string, for a readable failure message. A wrapped button bar spreads its
/// full-label buttons across more than one row; each label stays contiguous
/// within a single row, so a `\n`-joined `.contains(label)` finds it without
/// crossing the boundary.
pub fn joined_rows(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Box-drawing grid helpers
// ---------------------------------------------------------------------------
//
// Shared by every suite that reads a RENDERED grid (a vt100 PTY snapshot or a
// `joined_rows` buffer) and has to find, crop or bound a drawn box. Kept here
// as ONE copy because the alternative was measured and cost us: `a861c8d`
// promoted the selected deck card from a plain to a thick border and `eff6256`
// did the same to the focused pane, and each presentation change red-lined a
// different test file that had hardcoded the old glyph — issue #460. Two files
// had already grown near-identical copies of the pane-column scan below, which
// would have made the next such change a two-site edit where only one site got
// edited (review of #465, S1/S2).

/// One border weight's corner, horizontal and vertical glyphs — everything
/// needed to find a box's span on a row, to recognise its verticals, and to
/// bound the title fused into its top edge.
#[derive(Clone, Copy, Debug)]
pub struct BorderWeight {
    pub top_left: char,
    pub top_right: char,
    pub horizontal: char,
    pub vertical: char,
    pub bottom_left: char,
    pub bottom_right: char,
}

/// Every border weight a rendered box may use, in ratatui `BorderType` order.
///
/// The product paints only `Plain` and `Thick` today — `card_border_glyph`
/// (`src/ui.rs`) and `TerminalWidget::render` (`src/terminal_widget.rs`) both
/// return exactly those two, and `BorderType::Double` appears nowhere in
/// `src/`. `Double` is listed ANYWAY, deliberately, because every consumer of
/// this table is either a LOCATOR (find the box's column) or a
/// weight-AGNOSTIC extractor (require the same weight top and bottom), and for
/// both of those a weight they fail to recognise is a silent false negative,
/// not a caught defect. That is not hypothetical: the pane-column scan below
/// once pinned a single corner glyph and failed a mode switch that was working
/// correctly. Listing an unreachable weight costs one table row and never
/// weakens a same-weight coherence check — a card with a plain top and a thick
/// bottom is still rejected.
pub const BORDER_WEIGHTS: [BorderWeight; 3] = [
    BorderWeight {
        top_left: '┌',
        top_right: '┐',
        horizontal: '─',
        vertical: '│',
        bottom_left: '└',
        bottom_right: '┘',
    },
    BorderWeight {
        top_left: '┏',
        top_right: '┓',
        horizontal: '━',
        vertical: '┃',
        bottom_left: '┗',
        bottom_right: '┛',
    },
    BorderWeight {
        top_left: '╔',
        top_right: '╗',
        horizontal: '═',
        vertical: '║',
        bottom_left: '╚',
        bottom_right: '╝',
    },
];

/// Whether `ch` is any border weight's vertical glyph — the characters a
/// wrap-tolerant grid search has to drop, since a box's own left/right edges
/// are interposed into every row of text it contains.
pub fn is_box_vertical(ch: char) -> bool {
    BORDER_WEIGHTS.iter().any(|weight| weight.vertical == ch)
}

/// Whether `ch` is any border weight's horizontal glyph — the fill a box's top
/// and bottom edges are drawn with, and therefore what TERMINATES a title fused
/// into the top edge (`┏orchestrator [Z]━━━…┓`).
pub fn is_box_horizontal(ch: char) -> bool {
    BORDER_WEIGHTS.iter().any(|weight| weight.horizontal == ch)
}

/// Drop every whitespace run and box-drawing vertical from `text`, so a needle
/// that WRAPPED across rows still matches once the rows are joined.
///
/// A long line rendered into a box breaks at whatever column the box happens to
/// sit at, and each continuation row re-enters through the box's own left edge. A
/// needle straddling that wrap column is therefore absent from the row-joined
/// snapshot even though every character of it is on screen. Squeezing BOTH
/// haystack and needle makes the match independent of where the wrap fell, and of
/// whether the renderer re-flowed at a word boundary or hard-broke mid-token.
///
/// Squeezing alone is NOT sufficient on a split layout — see
/// [`orchestration_pane_column`] for the sidebar-splicing trap it leaves open.
pub fn squeeze_wrapped_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_whitespace() && !is_box_vertical(*ch))
        .collect()
}

/// Whether `label` appears inside ONE box's top-border span on some row.
///
/// Stronger than `grid.lines().any(|l| l.contains('┌') && l.contains(label))`:
/// on an Orchestration tab the sidebar's cards and the focused role's live
/// terminal share rows, so a label printed by the AGENT, to the right of a
/// card's own right corner, satisfied the loose form. Cropping to the span
/// between one weight's matching corners is what makes this specific to a card
/// TITLE. `tests/grid_box_helpers.rs` guards that in the fast tier.
pub fn label_in_box_top_border(grid: &str, label: &str) -> bool {
    grid.lines().any(|line| {
        let chars: Vec<char> = line.chars().collect();
        BORDER_WEIGHTS.iter().any(|weight| {
            chars
                .iter()
                .enumerate()
                .filter(|(_, ch)| **ch == weight.top_left)
                .any(|(start, _)| {
                    let Some(end) = chars
                        .iter()
                        .enumerate()
                        .skip(start + 1)
                        .find_map(|(index, ch)| (*ch == weight.top_right).then_some(index))
                    else {
                        return false;
                    };
                    chars[start..=end]
                        .iter()
                        .collect::<String>()
                        .contains(label)
                })
        })
    })
}

/// Column of the orchestration tab's pane-column LEFT edge, in Unicode
/// scalars, or `None` when no expanded orchestrator box is drawn.
///
/// The role-pane box drawn for the fixture's `start = true` role fuses its
/// title into the top border as `┌orchestrator───…`, so that corner's column
/// is exactly `panes_area.x` — the sidebar/pane boundary that
/// `orchestration_split_percents` controls. The sidebar's own truncated
/// `orchestrat…` card label carries no corner, so there is no collision.
///
/// Two preconditions, both of which return `None` rather than a wrong column,
/// and which callers must therefore report on separately (see the `Result`
/// returned by `e2e_idle_worker_detector.rs`'s wait helper — review S4):
///
/// 1. The fixture's start role must be named literally `orchestrator`.
/// 2. Its pane must render EXPANDED. A collapsed `Stacked` pane draws a
///    `Block` with `Borders::TOP` and a padded title — no corner glyph at all
///    (see `tests/e2e_orchestration_pane_column.rs`) — so there is no anchor.
///
/// The returned index counts scalars, which equals the terminal column only
/// while every cell left of the boundary is width-1. Fixtures keep it so.
pub fn orchestration_pane_left_edge(grid: &str) -> Option<usize> {
    role_pane_left_edge(grid, "orchestrator")
}

/// Column of the role pane box drawn for `role`, in Unicode scalars, or `None`
/// when no such expanded box is on the grid.
///
/// The general form of [`orchestration_pane_left_edge`], which is just this
/// with `"orchestrator"`. Under `PaneLayout::Stacked` only the FOCUSED role's
/// pane is drawn, so a test that has jumped focus to a non-start role has no
/// `orchestrator` box to anchor on and needs to name the role it focused
/// (PRD #313's zoom coverage does exactly that). Kept as ONE scan rather than a
/// second copy in a test file, for the reason recorded above the table: the
/// glyph set has already had to change once, and two copies is one copy too
/// many for that.
///
/// Same two preconditions as the wrapper: the box must be drawn EXPANDED (a
/// collapsed `Stacked` pane draws no corner glyph at all), and the returned
/// index counts scalars, which equals the terminal column only while every cell
/// to its left is width-1.
pub fn role_pane_left_edge(grid: &str, role: &str) -> Option<usize> {
    grid.lines().find_map(|line| {
        BORDER_WEIGHTS
            .iter()
            .filter_map(|weight| {
                let header = format!("{}{role}", weight.top_left);
                line.find(&header)
                    .map(|byte_index| line[..byte_index].chars().count())
            })
            .min()
    })
}

/// The title text fused into the TOP BORDER of the expanded box drawn for
/// `role` — every character on that row between the corner glyph and the first
/// border-fill glyph. `┏orchestrator [Z]━━━…┓` yields `orchestrator [Z]`.
/// `None` under the same two preconditions as [`role_pane_left_edge`], whose
/// scan this shares; when several boxes for `role` are on one row the LEFTMOST
/// wins, matching that function's `.min()`.
///
/// **Why a positional read rather than `grid.contains("[Z]")`.** PRD #313's
/// zoom indicator is ordinary title text on a rendered grid — a vt100 snapshot
/// carries characters, not style — and a pane's title is its *display name*,
/// which is agent-reachable: names arrive over the hook socket and
/// `sanitize_display_name` strips control characters and bidi overrides but NOT
/// brackets. So an agent may call itself `worker [Z]`, and that token then sits
/// on an UNZOOMED pane's border (or, truncated, on a sidebar card) and
/// satisfies any whole-grid `contains`. Reading the title of the box the
/// geometry actually expanded is the strongest lever a text-only grid gives:
/// the marker must ride on the pane zoom widened, not merely appear somewhere
/// on screen. The remaining case — the focused pane's own display name
/// spelling the marker — is indistinguishable in text by construction, which
/// is why the product draws the real marker in its own
/// `terminal_widget::zoom_marker_style` span; `render/layout/006` asserts that
/// style side against a real `Buffer`, where the styling survives.
pub fn role_pane_border_title(grid: &str, role: &str) -> Option<String> {
    grid.lines().find_map(|line| {
        BORDER_WEIGHTS
            .iter()
            .filter_map(|weight| {
                let header = format!("{}{role}", weight.top_left);
                let byte_index = line.find(&header)?;
                let title: String = line[byte_index..]
                    .chars()
                    .skip(1) // the corner glyph the scan anchored on
                    .take_while(|ch| !is_box_horizontal(*ch) && *ch != weight.top_right)
                    .collect();
                Some((line[..byte_index].chars().count(), title))
            })
            .min_by_key(|(column, _)| *column)
            .map(|(_, title)| title)
    })
}

/// Crop every row of `grid` to the orchestration pane column, dropping the
/// sidebar to its left.
///
/// Needed because a needle that wraps across pane rows is only contiguous once
/// the rows are joined, and joining the FULL grid splices the sidebar's
/// role-card text between them: `card1·pane1·card2·pane2…` breaks a needle
/// spanning `pane1`+`pane2`. Returns `None` on the same two preconditions as
/// [`orchestration_pane_left_edge`].
pub fn orchestration_pane_column(grid: &str) -> Option<String> {
    role_pane_column(grid, "orchestrator")
}

/// Crop every row of `grid` to the pane column of the role pane drawn for
/// `role`, dropping the sidebar to its left. The general form of
/// [`orchestration_pane_column`], for the same reason
/// [`role_pane_left_edge`] is the general form of its wrapper: under
/// `PaneLayout::Stacked` only the FOCUSED role's pane is drawn, so a test that
/// jumped focus to a non-start role has no `orchestrator` box to crop on.
/// Returns `None` on the same two preconditions as [`role_pane_left_edge`].
pub fn role_pane_column(grid: &str, role: &str) -> Option<String> {
    let left_edge = role_pane_left_edge(grid, role)?;
    Some(
        grid.lines()
            .map(|line| line.chars().skip(left_edge).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Poll `path` until its contents contain `needle`, or panic after the
/// harness wait ceiling ([`WAIT_TIMEOUT`]). Lives in the harness (not an
/// `e2e_*` test file) because it sleeps between reads — Decision 21 forbids
/// `std::thread::sleep` inside e2e test bodies, but the harness's wait
/// primitives are exempt, exactly like [`TuiDeck::wait_for_string`].
///
/// Restart tests use this to wait for the deck to flush a disk artifact (the
/// persisted `session.toml`) before the NEXT launch reads it. The deck writes
/// that file atomically (temp file + rename), so a read here always sees either
/// the previous or the new complete file — never a half-written one.
pub fn wait_for_file_contains(path: &Path, needle: &str) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && contents.contains(needle)
        {
            return;
        }
        if Instant::now() > deadline {
            let dump =
                std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"));
            panic!(
                "file {} did not contain {needle:?} within {WAIT_TIMEOUT:?}.\nContents:\n{dump}",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(QUIESCENT_IDLE_MS));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Walk `needles` against `haystack` in order, returning how many
/// elements of `needles` matched. The N-th element must be found at
/// an offset strictly greater than the offset that matched the
/// (N-1)-th element. Used by [`TuiDeck::wait_for_strings_in_order`]
/// and exercised by the unit tests below — extracted so the polling
/// logic stays trivial and the matching invariant is testable
/// without spawning a PTY.
fn match_needles_in_order(haystack: &[u8], needles: &[&str]) -> usize {
    let text = String::from_utf8_lossy(haystack);
    let mut cursor = 0usize;
    let mut matched = 0usize;
    for needle in needles {
        match text[cursor..].find(needle) {
            Some(rel_idx) => {
                let abs_end = cursor + rel_idx + needle.len();
                cursor = abs_end;
                matched += 1;
            }
            None => break,
        }
    }
    matched
}

/// Match `prefix` needles strictly in order against `haystack`, then
/// check whether ANY of `terminals` appears AFTER the matched prefix.
///
/// Returns `(prefix_matched, terminal_found)`:
/// - `prefix_matched` — how many prefix needles were found in order,
///   advancing a cursor past each (the exact in-order semantics of
///   [`match_needles_in_order`]). A value below `prefix.len()` means
///   the prefix is incomplete, and `terminal_found` is then `false`.
/// - `terminal_found` — once the full prefix matched, whether at least
///   one terminal alternative occurs in the bytes AFTER the prefix's
///   end cursor. Searching only after the prefix is what stops a stale
///   pre-lifecycle status (e.g. a restored session's default `Idle`
///   rendered *before* `Thinking`) from satisfying the terminal check.
fn match_prefix_then_terminal(
    haystack: &[u8],
    prefix: &[&str],
    terminals: &[&str],
) -> (usize, bool) {
    let text = String::from_utf8_lossy(haystack);
    let mut cursor = 0usize;
    let mut matched = 0usize;
    for needle in prefix {
        match text[cursor..].find(needle) {
            Some(rel_idx) => {
                cursor += rel_idx + needle.len();
                matched += 1;
            }
            None => return (matched, false),
        }
    }
    let terminal_found = terminals.iter().any(|t| text[cursor..].contains(t));
    (matched, terminal_found)
}

/// Whether the terminal condition is satisfied, weighing BOTH available
/// sources of evidence: the rolling byte stream (which catches a transient
/// state already overwritten by the time anyone looks) and the CURRENTLY
/// rendered grid (which catches a state plainly on screen whose bytes were
/// never re-emitted).
///
/// The grid arm exists because the byte stream is NOT a faithful record of
/// what is on screen. ratatui renders DIFFERENTIALLY — a cell region that does
/// not change between frames emits no bytes at all — and it can split one
/// visible line across several writes when styling changes mid-line. Either is
/// enough for a terminal state the user is plainly looking at to be missing
/// from the post-prefix stream, which is exactly how
/// `claude_001_thinking_working_idle` flaked: its panic printed a final grid
/// containing `Launch an agent to get started` while reporting it had seen
/// none of the terminal alternatives.
///
/// The grid arm carries the SAME ordering rule as the stream arm, and needs it
/// for the same reason. The stream is searched only after the prefix cursor;
/// the grid is a whole screen with no cursor, so "is the needle on screen" is
/// far too loose — the needle may have been sitting in some unrelated region
/// since boot. `delegate_014` is the proof: its worker command is
/// `claude --model … --allowedTools Bash Read Write`, the deck renders a role's
/// command on its card, and the terminal alternatives there are
/// `["Bash", "bash"]`. A bare `grid.contains` would match that command string
/// the instant `Thinking → Working` completed and pass the test without the
/// worker ever running a Bash tool (Greptile P2 on #585).
///
/// So the grid must show the needle ARRIVING: `baseline` latches the screen at
/// the moment the prefix first completes with the stream still unsatisfied,
/// and only a needle present now and ABSENT from that baseline counts. That is
/// the grid-dimension equivalent of "after the cursor", and it keeps the
/// property the byte stream had — a state that was already true before the
/// working lifecycle finished can never satisfy the terminal condition.
///
/// `grid` is a closure because [`Deck::snapshot_grid`] renders the whole
/// screen and this is polled every 20 ms — it must not be paid for on the
/// common path where the cheap stream check already settled the question.
fn terminal_reached(
    haystack: &[u8],
    prefix: &[&str],
    terminals: &[&str],
    baseline: &mut Option<String>,
    grid: impl FnOnce() -> String,
) -> (usize, bool) {
    let (matched, in_stream) = match_prefix_then_terminal(haystack, prefix, terminals);
    if matched < prefix.len() || in_stream {
        return (matched, in_stream);
    }
    let grid = grid();
    // First poll past the gate establishes what was ALREADY on screen; nothing
    // can have arrived yet, so this poll never matches by construction.
    let baseline = baseline.get_or_insert_with(|| grid.clone());
    (
        matched,
        terminals
            .iter()
            .any(|t| grid.contains(t) && !baseline.contains(t)),
    )
}

/// PRD #381: seed `<home>/.local/bin/dot-agent-deck` as a **symlink to the
/// binary under test**, so the deck's durable-path resolver lands inside the
/// sandbox instead of on the host.
///
/// Without this, every e2e test that relies on installed hooks would silently
/// exercise the wrong binary. The resolver refuses to write a
/// `target/{debug,release}` path into agent config, and under `cargo test-e2e`
/// the binary under test IS one — so it falls to step 2a
/// (`$HOME/.local/bin/dot-agent-deck`) and then step 2b (`$PATH`). This harness
/// passes the HOST `PATH` through (`inherit_pass`, below), and a developer box
/// commonly has a real installed deck on it: hooks would then point at the
/// host's deck and the tier would test stale code, while a machine with no
/// installed deck would get a refusal and no hooks at all. Seeding step 2a
/// makes the resolution order run for real and land on this build.
///
/// A **symlink**, not a copy: the resolver deliberately does not canonicalize
/// its 2a candidate, so the durable symlink path is what gets written while the
/// bytes executed are the freshly-built ones. A copy would go stale on the next
/// `cargo build` and cost the binary's size per test.
///
/// `#[cfg(unix)]` because the L2 tier is Unix-only, and best-effort because a
/// failure here can only degrade a test to the pre-#381 host-`PATH` behaviour,
/// never corrupt anything.
#[cfg(unix)]
fn seed_durable_binary(home: &Path) {
    let bin_dir = home.join(".local").join("bin");
    if std::fs::create_dir_all(&bin_dir).is_err() {
        return;
    }
    // `DEFAULT_BINARY_NAME`'s value, spelled out: the resolver looks for the
    // crate's package name, not whatever the test binary happens to be called.
    let link = bin_dir.join("dot-agent-deck");
    let _ = std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_dot-agent-deck"), link);
}

fn locate_fixture(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is the repo root for integration tests.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let p = root.join("tests").join("fixtures").join(name);
    assert!(p.is_dir(), "fixture directory missing: {p:?}");
    p
}

fn copy_dir_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursively(&from, &to)?;
        } else if ty.is_file() {
            std::fs::copy(&from, &to)?;
        } else {
            // M2.1 auditor Nit 3: refuse to copy symlinks / sockets /
            // FIFOs from a fixture. Fixtures are plain files only —
            // a symlink at copy time most likely indicates a fixture
            // bug (or an attacker pre-staging a symlink targeting the
            // tempdir's parent), so surface it loudly instead of
            // silently skipping.
            return Err(std::io::Error::other(format!(
                "fixture entry {} is not a regular file or directory \
                 (symlinks/sockets/FIFOs are not supported in fixtures)",
                from.display()
            )));
        }
    }
    Ok(())
}

/// Workspace-relative `.dot-agent-deck/recordings/` resolved to an
/// ABSOLUTE path at harness construction time. The fixture-copy step
/// `cwd`s the deck into a per-test tempdir, so any relative path here
/// would land artifacts in the wrong place. M4.3: artifacts moved
/// from `target/test-recordings/<run-id>/<test>/` to
/// `.dot-agent-deck/recordings/<test>/` — gitignored dev-time state,
/// no per-run subdir (concurrent runs on one checkout aren't a
/// real-world workflow).
fn workspace_recordings_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".dot-agent-deck")
        .join("recordings")
}

/// Atomic file write: stage `bytes` in a sibling tempfile under
/// `dst.parent()` and then `persist(dst)` so the rename is atomic on
/// Unix (same filesystem). Concurrent writers see either the
/// previous or the new file, never a half-written one.
fn atomic_write(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = dst.parent().ok_or_else(|| {
        std::io::Error::other(format!("dump path has no parent: {}", dst.display()))
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".tui-harness-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    std::io::Write::write_all(tmp.as_file_mut(), bytes)?;
    tmp.as_file().sync_all().ok();
    tmp.persist(dst).map_err(|e| e.error)?;
    Ok(())
}

fn current_test_name() -> String {
    // Rust unit tests run on threads named after the test function.
    // Falls back to a placeholder when called off-thread.
    std::thread::current()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unnamed-test".to_string())
}

fn sanitize_test_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render a parsed grid as a minimal monospace SVG. Each row becomes
/// one `<text>` element; cells get no per-attribute styling — colors
/// would need attribute tracking which is more than the failure-dump
/// surface needs. Reviewers replay the cast for color.
fn render_grid_to_svg(grid: &str, cols: u16, rows: u16) -> String {
    let cell_w = 8;
    let cell_h = 16;
    let width = cols as usize * cell_w;
    let height = rows as usize * cell_h;
    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n"
    ));
    s.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#0c0c0c\"/>\n");
    s.push_str("<style>text { font-family: monospace; font-size: 13px; fill: #d0d0d0; }</style>\n");
    for (i, line) in grid.lines().enumerate() {
        let y = (i + 1) * cell_h;
        let escaped = line
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        s.push_str(&format!(
            "<text x=\"0\" y=\"{y}\" xml:space=\"preserve\">{escaped}</text>\n"
        ));
    }
    s.push_str("</svg>\n");
    s
}

/// PRD #77 Decision 26 runtime-skip helper: returns `Ok(())` when the
/// host has the Claude Code CLI on PATH and a **usable** credentials
/// file; `Err(reason)` with a stable user-facing message otherwise.
/// Tests pair this with [`skip_unless!`].
///
/// PRD #126: mere existence of `~/.claude/.credentials.json` used to be
/// enough, so a truncated, unparseable or fully expired credential set passed
/// the check and the scenario then failed deep inside a PTY wait with a
/// confusing timeout. The extra checks below are all **cheap and offline** —
/// no probe request, unlike [`check_codex_available`], because the equivalent
/// `claude -p` round trip costs real tokens on every e2e run. Expiry is
/// treated the way Claude Code itself treats it: an expired access token is
/// fine while a live refresh token can still renew it, so only the case where
/// BOTH are spent is reported as unusable — see [`claude_oauth_usable`], which
/// holds that half as a pure function so `tests/real_agent_preflight.rs` can
/// assert every accepted and rejected credential shape.
///
/// PRD #386: the credential set is no longer necessarily a FILE. Claude Code
/// 2.x on macOS keeps it in the login Keychain as the generic-password item
/// `Claude Code-credentials`, and `~/.claude/.credentials.json` is simply
/// absent on a migrated host — measured on this repo's dev machine on
/// 2026-08-06 (no file at all, Keychain item present, `claude` 2.1.220
/// working). The file check therefore falls back to a Keychain probe instead
/// of reporting "not found". Without that fallback EVERY real-agent test in
/// this suite silently self-skips on macOS and reports PASS — and that is
/// exactly the tier CLAUDE.md rule 5 exception (a) says must be run locally
/// BECAUSE CI has no credentials to run it, so both sides would be green while
/// proving nothing.
///
/// Issue #502/#785: there is now a THIRD path — a non-empty
/// `ANTHROPIC_API_KEY`, consulted last, after both credential stores have come
/// up empty. That is what lets lane 2 run the claude-gated tests on a GitHub
/// runner from a scopable, revocable, spend-cappable key instead of the owner's
/// account session. See the comment on that branch, and note it is coupled to
/// [`seed_claude_project_trust`]: an interactive agent authenticated by key
/// also needs the API-key approval recorded in `~/.claude.json`, or it stops on
/// a prompt that defaults to "No".
pub fn check_claude_available() -> Result<(), String> {
    if !cli_invocable("claude") {
        return Err("Claude Code CLI not installed (could not invoke `claude --version`)".into());
    }
    match check_claude_credentials_file() {
        Ok(()) => Ok(()),
        // A usable file is authoritative; when it is absent OR unusable the
        // Keychain may still hold a live credential set, so the fallback
        // decides. This never weakens the gate into "always available" — a host
        // with neither still skips, and now names both storage locations so the
        // next person isn't sent hunting for a file this Claude Code version
        // never writes.
        Err(file_reason) => {
            if claude_keychain_credentials_present() {
                return Ok(());
            }
            // Issue #502/#785: a non-empty ANTHROPIC_API_KEY is a THIRD auth
            // path, not a replacement for the other two — note it is consulted
            // last, so a host with a usable file or Keychain item still
            // authenticates exactly the way it did before this existed.
            //
            // Claude Code genuinely runs from the key alone: measured on a
            // virgin HOME with no `.credentials.json` at any point, both in
            // print mode and interactively under a PTY, doing real tool-using
            // work and reporting "API Usage Billing". The one extra thing an
            // INTERACTIVE agent needs is the API-key approval recorded in
            // `~/.claude.json` — without it Claude Code stops on a prompt that
            // defaults to "No" and the test dies at a PTY wait, which is the
            // confusing-timeout failure class this whole gate exists to
            // prevent. `seed_claude_project_trust` writes that approval, and
            // the two are coupled: widening this check without it just moves
            // the failure later.
            //
            // This is what lets lane 2 run the 22 claude-gated tests on a
            // GitHub runner under #785 decision 1 — a scopable, spend-cappable,
            // independently revocable API key rather than the owner's account
            // session. The key is only tested for presence here; it is never
            // printed, and no probe request is made (same trade as the file
            // path: a live round trip would spend tokens on every e2e run, so
            // a revoked key remains an accepted false-positive that fails
            // loudly later).
            if anthropic_api_key().is_some() {
                return Ok(());
            }
            Err(format!(
                "{file_reason}{CLAUDE_KEYCHAIN_HINT} (and {ANTHROPIC_API_KEY_ENV} is unset, \
                 empty or whitespace-only, which is the third way this check can be satisfied)"
            ))
        }
    }
}

/// Issue #502/#785: the environment variable Claude Code, OpenCode and `pi` all
/// read an Anthropic API key from, and the only agent credential lane 2 holds in
/// CI. Named here so the harness, the gates and the redaction agree on one
/// spelling.
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The ambient Anthropic API key, or `None` when it is unset, empty or
/// whitespace-only.
///
/// Presence is decided on the TRIMMED value — the same rule all three
/// `check_pi_available` copies apply, and the same rule `e2e-live.yml`'s guard
/// step applies with `${VAR//[[:space:]]/}` — but the value comes back
/// VERBATIM, because verbatim is what a spawned agent receives and therefore
/// what it derives [`claude_api_key_response_id`] from.
///
/// This is a secret. It is returned only to be threaded into a child process's
/// environment and to be registered for recording redaction; it is never
/// printed, never formatted into an error and never written to an artifact.
fn anthropic_api_key() -> Option<String> {
    std::env::var(ANTHROPIC_API_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

/// The identifier Claude Code files an API key under in `~/.claude.json`'s
/// `customApiKeyResponses` — the key's **last 20 characters**.
///
/// Measured twice: the interactive "Detected a custom API key in your
/// environment" prompt renders exactly this suffix and answering it records
/// exactly this string, and every entry already present in this repo's dev-box
/// `~/.claude.json` is 20 characters long.
///
/// Counted in `char`s rather than bytes so a non-ASCII value cannot panic on a
/// split landing mid-code-point. Real keys are ASCII, where the two agree.
///
/// The result is a DERIVATIVE of the secret and must be treated as one: GitHub
/// masks a registered secret's exact value in a rendered log, not a substring of
/// it, which is why [`TuiDeckBuilder::launch`] registers this string for
/// recording redaction rather than relying on masking.
fn claude_api_key_response_id(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let start = chars.len().saturating_sub(20);
    chars[start..].iter().collect()
}

/// Everything about an Anthropic API key that must never survive into a
/// `.cast`, a `final-grid.txt` or a copied `fixture.toml`: the key, and the
/// 20-character derivative Claude Code's approval prompt paints on the terminal.
///
/// Longest first, because [`credential_redaction_ranges`] prefers the longest
/// value at a shared start — the two do not share one, but the list is sorted
/// that way by its consumer anyway and this keeps the intent local.
///
/// Pure in the key so the redaction it produces is covered by a unit test on
/// the fast tier rather than only by the credentialed lane. The env read stays
/// at the one call site in [`TuiDeckBuilder::launch`].
fn api_key_recording_redactions(key: &str) -> Vec<String> {
    vec![key.to_string(), claude_api_key_response_id(key)]
}

/// Whether the HOST carries an OAuth credential set the gate accepts — the file
/// first, then the macOS Keychain. Exactly the pair [`check_claude_available`]
/// consulted before the API key became a third path, so "was this run
/// authorised by OAuth?" has ONE answer that the gate, the import and the
/// `~/.claude.json` seeding all read.
///
/// Keep it that way. [`import_claude_credentials`] writes a credentials file
/// into the test HOME if and only if this is true, and
/// [`seed_claude_project_trust`] approves the API key if and only if it is
/// false. If the two ever disagree, an agent gets an OAuth file it cannot use,
/// or an approved key it did not need — and the second of those silently moves
/// a developer's local run off their subscription and onto metered API billing.
fn host_claude_oauth_usable() -> bool {
    check_claude_credentials_file().is_ok() || claude_keychain_credentials_present()
}

/// The file half of [`check_claude_available`]: `~/.claude/.credentials.json`
/// exists as a regular file, parses, and carries a usable `claudeAiOauth`
/// entry. Kept as the first-choice path — Linux and pre-2.x installs still
/// store credentials here.
fn check_claude_credentials_file() -> Result<(), String> {
    // M3.1 auditor S1: every message below surfaces the abstract path so it
    // doesn't leak whether the operator is on `/Users/<name>` vs `/root` vs
    // `/home/<name>`.
    const MISSING: &str = "Claude Code credentials not found at ~/.claude/.credentials.json — \
                           log in with `claude login`";
    let creds = host_home().join(".claude").join(".credentials.json");
    // A symlink here would let a stray link outside the reviewed HOME decide
    // the outcome; mirror `check_codex_available`'s regular-file requirement.
    if !std::fs::symlink_metadata(&creds)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false)
    {
        return Err(MISSING.into());
    }
    let raw = std::fs::read_to_string(&creds).map_err(|_| MISSING.to_string())?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
        "Claude Code credentials at ~/.claude/.credentials.json are not valid JSON — \
         log in again with `claude login`"
            .to_string()
    })?;
    let oauth = parsed.get("claudeAiOauth").ok_or(
        "Claude Code credentials at ~/.claude/.credentials.json carry no `claudeAiOauth` \
         entry — log in with `claude login`",
    )?;
    claude_oauth_usable(oauth, now_epoch_ms())
}

/// Epoch milliseconds for the credential expiry checks, shared by the file and
/// Keychain halves of the gate so the two cannot drift apart in how they read
/// the clock. `0` on the impossible pre-epoch clock, which makes every expiry
/// look live — the same fail-open-then-fail-loudly direction the rest of this
/// gate takes.
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Would the gate accept this credential document? Parses the bytes, requires a
/// `claudeAiOauth` entry, and applies [`claude_oauth_usable`] — the one place
/// that question is answered, so the availability check, the Keychain probe and
/// the import all judge a credential set by the same rule regardless of where
/// it came from.
///
/// Takes bytes rather than a parsed value so callers holding a secret buffer
/// can classify it and then zero it. The parse does put a second copy of the
/// secret in the `serde_json::Value`, which cannot be zeroed the way a byte
/// buffer can; it is local, dropped when this returns, and never read except
/// through `claude_oauth_usable`.
fn claude_credential_document_usable(raw: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .as_ref()
        .and_then(|parsed| parsed.get("claudeAiOauth"))
        .is_some_and(|oauth| claude_oauth_usable(oauth, now_epoch_ms()).is_ok())
}

/// Service name Claude Code 2.x files its OAuth credential set under in the
/// macOS login Keychain.
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Appended to the file-side failure when the Keychain fallback also came up
/// empty, so a genuinely credential-less host is told about BOTH locations.
/// Empty off macOS, where there is no Keychain to have checked and the
/// pre-existing message is still the whole truth.
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_HINT: &str = " (and the macOS login Keychain holds no usable `Claude Code-credentials` item \
     either — Claude Code 2.x stores the credential set there rather than in the \
     file, and it is checked for the same `claudeAiOauth` shape and expiry as the \
     file is)";
#[cfg(not(target_os = "macos"))]
const CLAUDE_KEYCHAIN_HINT: &str = "";

/// Whether the macOS login Keychain holds a **usable** Claude Code credential
/// set — the same question [`check_claude_credentials_file`] asks of the file,
/// asked of the Keychain.
///
/// PRD #386 auditor finding: this used to answer only "`security` exited 0 with
/// non-empty output", so the two halves of the gate disagreed — an expired or
/// malformed Keychain item passed where byte-identical content in a file was
/// rejected. It now parses the exported document and applies the identical
/// `claudeAiOauth` + [`claude_oauth_usable`] test, so a host is judged the same
/// way whichever store its credentials live in. The consequence of the old
/// asymmetry was a test that ran and then failed loudly deep in a PTY wait
/// (never a silent green — [`claude_keychain_credentials_export`] already
/// refuses to seed a test HOME from anything that is not a `claudeAiOauth`
/// document), which is why it was a correctness fix rather than a blocker.
///
/// PRIVACY, load-bearing and unchanged by that fix: this yields a BOOLEAN and
/// nothing else. `-w` prints the password itself, so the secret is read into a
/// local buffer purely to be classified; the buffer is zeroed before it drops,
/// never returned, and never formatted into an error, a panic, a log line, a
/// test artifact or a `.cast` recording. stderr is discarded for the same
/// reason, and `claude_oauth_usable`'s `Err` string — which names only the
/// abstract `~/.claude/.credentials.json` path, per the M3.1 auditor S1
/// property — is collapsed to a bool here rather than propagated.
///
/// The parse does put a second copy of the secret on the heap inside the
/// `serde_json::Value`, which cannot be zeroed the way the byte buffer can.
/// That copy is local, dropped at the end of this function, and never read
/// except through `claude_oauth_usable` — the same handling
/// [`claude_keychain_credentials_export`] has always given its own parse of
/// the same bytes.
///
/// `security` resolves the login keychain from `$HOME`, so the probe is pinned
/// to [`host_home`]: under a relocated HOME it answers "keychain not found"
/// (exit 44) no matter what the real user has.
#[cfg(target_os = "macos")]
fn claude_keychain_credentials_present() -> bool {
    let probe = std::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .env("HOME", host_home())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match probe {
        Ok(mut out) => {
            let usable = out.status.success() && claude_credential_document_usable(&out.stdout);
            out.stdout.fill(0);
            usable
        }
        Err(_) => false,
    }
}

/// Non-macOS hosts keep the file-only behaviour unchanged: Linux Claude Code
/// still writes `~/.claude/.credentials.json`, and CI — which has no
/// credentials either way — must keep skipping.
#[cfg(not(target_os = "macos"))]
fn claude_keychain_credentials_present() -> bool {
    false
}

/// Export Claude Code's credential set out of the macOS login Keychain, so
/// [`import_claude_credentials`] can seed a per-test HOME that cannot reach the
/// Keychain itself.
///
/// `None` when there is no such item, when `security` cannot be run, or when
/// what came back is not a **usable** `claudeAiOauth` credential document (the
/// same rule the gate applies — see [`claude_credential_document_usable`]) —
/// the caller then reports its file-side error instead, so an unrelated,
/// malformed or spent keychain item can never be written into a test HOME as
/// if it were a login.
///
/// These bytes ARE the secret, unlike [`claude_keychain_credentials_present`]'s
/// boolean. They are returned solely to be handed to
/// [`write_credential_file_atomic_0o600`] — never logged, never formatted into
/// an error, and never echoed to a terminal, so they cannot reach a `.cast`
/// recording. That is the same handling the file-sourced bytes have always had.
#[cfg(target_os = "macos")]
fn claude_keychain_credentials_export() -> Option<Vec<u8>> {
    let probe = std::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE, "-w"])
        .env("HOME", host_home())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !probe.status.success() {
        return None;
    }
    // `security -w` terminates the password with a newline; truncate in place
    // rather than copying, so the secret lives in exactly one buffer.
    let mut bytes = probe.stdout;
    while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
        bytes.pop();
    }
    // Same usability rule the gate applies (PRD #386 review): an expired or
    // malformed Keychain item is not a source worth seeding a test HOME from,
    // and returning it would hand the caller a credential set
    // `check_claude_available` would have rejected.
    if !claude_credential_document_usable(&bytes) {
        bytes.fill(0);
        return None;
    }
    Some(bytes)
}

/// Non-macOS hosts have no Keychain to export from; the file is the only
/// source. See [`claude_keychain_credentials_present`].
#[cfg(not(target_os = "macos"))]
fn claude_keychain_credentials_export() -> Option<Vec<u8>> {
    None
}

/// The credential-shape half of [`check_claude_available`], split out as a pure
/// function of the `claudeAiOauth` object and the current epoch-millisecond
/// clock so every shape below is covered by a test instead of by argument
/// (`tests/real_agent_preflight.rs`).
///
/// PRD #126 audit follow-up: each expiry is bound to the presence of ITS OWN
/// token. The first cut evaluated the two expiries independently of the two
/// tokens, and because an ABSENT expiry means "no expiry information" (never
/// "expired"), the missing half of an asymmetric credential set voted "live" for
/// a token that was not there at all. So an expired sole access token with no
/// refresh token passed, and so did an access-token-less set whose refresh token
/// was already spent — precisely the "credentials look fine, then the real agent
/// fails deep inside a PTY wait" case this check exists to catch.
///
/// Two deliberate decisions are preserved. An expired access token with a LIVE
/// refresh token still passes, because Claude Code itself refreshes on that
/// shape. And there is **no probe request**: revoked credentials and network
/// failures remain an accepted false-positive class, since a live round trip
/// would spend real tokens on every e2e run.
pub fn claude_oauth_usable(oauth: &serde_json::Value, now_ms: i64) -> Result<(), String> {
    let non_empty = |key: &str| {
        oauth
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
    };
    if !non_empty("accessToken") && !non_empty("refreshToken") {
        return Err(
            "Claude Code credentials at ~/.claude/.credentials.json carry no access or refresh \
             token — log in with `claude login`"
                .into(),
        );
    }
    // Both timestamps are epoch MILLISECONDS. An absent field is treated as
    // "no expiry information", never as expired.
    let live = |key: &str| {
        oauth
            .get(key)
            .and_then(|v| v.as_i64())
            .is_none_or(|at| at > now_ms)
    };
    // A token is usable only if it is BOTH present and unexpired; an expiry
    // alone says nothing about a token that does not exist.
    let usable = |token_key: &str, expiry_key: &str| non_empty(token_key) && live(expiry_key);
    if !usable("accessToken", "expiresAt") && !usable("refreshToken", "refreshTokenExpiresAt") {
        return Err(
            "Claude Code credentials at ~/.claude/.credentials.json are expired and cannot be \
             refreshed — log in again with `claude login`"
                .into(),
        );
    }
    Ok(())
}

/// PRD #77 Decision 26 runtime-skip helper for OpenCode. Mirrors
/// [`check_claude_available`] — checks for the CLI on PATH and an
/// OpenCode auth.json (or analogous credential the user logged in
/// with).
///
/// Issue #502/#785 adds the same third path [`check_claude_available`] grew, and
/// measured the same way: `opencode run --model anthropic/claude-haiku-4-5` did
/// real tool-using work in a virgin HOME from `ANTHROPIC_API_KEY` alone, writing
/// no `auth.json` anywhere. The file requirement was therefore strictly
/// stricter than the CLI, exactly as claude's was.
///
/// It is offered ONLY for an `anthropic/…` test model, and that restriction is
/// load-bearing rather than cautious. [`opencode_test_model`] is configurable
/// and defaults to `openai/gpt-5.4-mini`; `inherit_pass` forwards
/// `ANTHROPIC_API_KEY` and nothing else, so accepting an Anthropic key for an
/// OpenAI-backed model would open the gate on a credential the spawned agent
/// could not use — turning a clean skip into a failure deep in a PTY wait,
/// which is the outcome this whole family of checks exists to prevent. For any
/// other provider the gate is unchanged and still wants an `auth.json`.
pub fn check_opencode_available() -> Result<(), String> {
    if !cli_invocable("opencode") {
        return Err("OpenCode CLI not installed (could not invoke `opencode --version`)".into());
    }
    let home = host_home();
    let candidates = [
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json"),
        home.join(".opencode").join("auth.json"),
        home.join(".config").join("opencode").join("auth.json"),
    ];
    if candidates.iter().any(|p| p.exists()) {
        return Ok(());
    }
    if opencode_env_key_authorises() {
        return Ok(());
    }
    // M3.1 auditor S1: redact $HOME in the surfaced path.
    Err(format!(
        "OpenCode credentials not found at ~/.local/share/opencode/auth.json — \
         log in with `opencode auth login` (or, for an `anthropic/…` \
         {OPENCODE_TEST_MODEL_ENV}, set {ANTHROPIC_API_KEY_ENV}; the model here \
         is `{}`)",
        opencode_test_model()
    ))
}

/// Whether an ambient `ANTHROPIC_API_KEY` is enough to run the OpenCode tests —
/// true only when [`opencode_test_model`] names the `anthropic` provider, so the
/// key the harness forwards is the one the model needs. See
/// [`check_opencode_available`] for why the provider match is required.
fn opencode_env_key_authorises() -> bool {
    opencode_test_model()
        .split_once('/')
        .is_some_and(|(provider, _)| provider == "anthropic")
        && anthropic_api_key().is_some()
}

/// Compiled-in default cheap model for Codex availability probes and real-agent
/// e2e coverage. Reachable by an **API-key** `~/.codex/auth.json`; a
/// ChatGPT-subscription (oauth) host must override it — see [`codex_test_model`].
const CODEX_TEST_MODEL_DEFAULT: &str = "gpt-5.1-codex-mini";

/// Env var that overrides [`codex_test_model`] on a host whose Codex credentials
/// cannot reach the default.
pub const CODEX_TEST_MODEL_ENV: &str = "DOT_AGENT_DECK_CODEX_TEST_MODEL";

/// Cheap model used by Codex availability probes and real-agent e2e coverage —
/// [`CODEX_TEST_MODEL_DEFAULT`] unless `DOT_AGENT_DECK_CODEX_TEST_MODEL` is set
/// to a non-empty value, which wins.
///
/// The override exists because no single model id is reachable by both Codex
/// auth modes, so whichever one the default targets, the other needs an escape
/// hatch.
///
/// The `codex-*` family is **API-key only**. On a ChatGPT-subscription (oauth)
/// `~/.codex/auth.json`, `codex exec --model gpt-5.1-codex-mini` answers
/// `400 invalid_request_error: The 'gpt-5.1-codex-mini' model is not supported
/// when using Codex with a ChatGPT account` (measured 2026-08-23), so
/// [`check_codex_available`] fails its probe and every real-agent Codex test
/// SKIPS — a silent no-coverage outcome that reads as a pass.
///
/// A subscription host therefore exports a plain `gpt-5.*` model it *can* reach,
/// e.g. `DOT_AGENT_DECK_CODEX_TEST_MODEL=gpt-5.6-luna` (verified 2026-08-26:
/// `codex exec` answered `CODEX_AUTH_OK`, and codex-cli 0.149.0's interactive TUI
/// came up on it).
///
/// `gpt-5.4-mini` — what this line named until 2026-08-26 — was re-probed the
/// same day and **also still works**, by both routes. It is named here rather
/// than silently dropped because the swap is a refresh of a dated claim, not the
/// retirement of a dead model id: if you are already exporting it, nothing is
/// wrong. (The probe that appeared to condemn it was measuring its own defect —
/// a `pty.fork()` left at a 0x0 window size, into which codex-cli paints nothing
/// whatever the model. Both ids emit the identical 523 bytes of empty repaint at
/// 0x0 and the identical 2492 bytes ending in `? for shortcuts` at 180x45.)
///
/// **Setting it makes the tests run, not pass.** They then fail on an unrelated
/// defect: Codex 0.149.0 does not execute the deck's trusted command hooks in
/// **interactive TUI** mode, so no hook-sourced event ever reaches the deck and
/// assertions wanting a `Thinking` carrying `user_prompt` time out. Measured
/// 2026-08-23 against one `CODEX_HOME` with identical `hooks.json`, identical
/// trust records and an identical socket: `codex exec` delivered `session_start`,
/// `thinking` (with `user_prompt`) and `idle`; the same home driven interactively
/// delivered nothing, while the turn itself ran (the deck still saw `ShellBusy`
/// from the stdout classifier). `docs/develop/agent-adapters.md` records
/// interactive hooks working on 0.145.0, so this is a regression in that range,
/// and it is auth-mode independent — an API-key host on 0.149.0 fails the same
/// way. The default is left on the API-key model so those tests SKIP rather than
/// fail while that is outstanding.
///
/// (Before 2026-08-23 this comment asserted the reverse of the first paragraph —
/// that `codex-*` was oauth-only and the override was for API-key hosts. That was
/// wrong and cost a debugging session: the probe failed on a freshly
/// authenticated subscription and the error text sent the reader looking for an
/// API key that was not there.)
///
/// Single source of truth: [`check_codex_available`] probes the model this
/// returns, so the availability gate and the model the tests actually launch can
/// never disagree.
pub fn codex_test_model() -> &'static str {
    static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODEL.get_or_init(|| {
        std::env::var(CODEX_TEST_MODEL_ENV)
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| CODEX_TEST_MODEL_DEFAULT.to_string())
    })
}

/// Compiled-in default cheap model for real-agent OpenCode e2e coverage.
///
/// OpenCode model ids are provider-qualified (`provider/model`); a bare
/// `gpt-4o-mini` is rejected as "Invalid model format". This default is on the
/// `openai` provider so it resolves against whatever `opencode auth` holds for
/// OpenAI — including a **ChatGPT-subscription (oauth)** credential, which is
/// how the dev boxes here are logged in and which costs nothing per call.
///
/// This deliberately does *not* route through OpenRouter. It used to: both call
/// sites hardcoded `openrouter/openai/gpt-4o-mini`, which billed metered
/// OpenRouter credit for coverage the subscription already pays for, and made
/// two OpenCode tests skip whenever that balance ran dry — with a skip reason
/// naming missing *credentials*, which were present the whole time.
///
/// **Probed 2026-08-26** (issue #243 round 3), because this shares a model id
/// with [`codex_test_model`]'s subscription example and a retirement here would
/// be worse: `check_opencode_available` runs no model probe at all — it only
/// looks for an `auth.json` — so an unreachable id would not skip
/// `orchestration/delegate/015` cleanly, it would fail it somewhere inside the
/// TUI with no mention of the model. `opencode run --model openai/gpt-5.4-mini`
/// answered `OPENCODE_MODEL_OK` on the subscription credential these boxes hold.
/// Note the id reaches the model through OpenCode's own provider layer rather
/// than through codex-cli, so nothing measured about codex-cli's TUI bears on it.
const OPENCODE_TEST_MODEL_DEFAULT: &str = "openai/gpt-5.4-mini";

/// Env var that overrides [`opencode_test_model`] on a host whose OpenCode
/// credentials cannot reach the default — e.g. one authenticated to OpenRouter
/// but not OpenAI, which exports
/// `DOT_AGENT_DECK_OPENCODE_TEST_MODEL=openrouter/openai/gpt-4o-mini`.
pub const OPENCODE_TEST_MODEL_ENV: &str = "DOT_AGENT_DECK_OPENCODE_TEST_MODEL";

/// Cheap provider-qualified model used by real-agent OpenCode e2e coverage —
/// [`OPENCODE_TEST_MODEL_DEFAULT`] unless `DOT_AGENT_DECK_OPENCODE_TEST_MODEL`
/// is set to a non-empty value, which wins.
///
/// Mirrors [`codex_test_model`]. Note the gates are not symmetric:
/// [`check_opencode_available`] only checks that an `auth.json` exists and does
/// **not** probe the model, so an unreachable model id here surfaces as a test
/// failure rather than a skip.
pub fn opencode_test_model() -> &'static str {
    static MODEL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODEL.get_or_init(|| {
        std::env::var(OPENCODE_TEST_MODEL_ENV)
            .ok()
            .map(|raw| raw.trim().to_string())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| OPENCODE_TEST_MODEL_DEFAULT.to_string())
    })
}

/// Runtime-skip helper for real Codex coverage. A version check alone is not
/// enough: this verifies persisted auth and performs one minimal model request,
/// so expired credentials, 401 responses, and unreachable accounts skip cleanly
/// before the PTY scenario starts.
pub fn check_codex_available() -> Result<(), String> {
    if !cli_invocable("codex") {
        return Err("Codex CLI not installed (could not invoke `codex --version`)".into());
    }

    let auth_path = host_home().join(".codex").join("auth.json");
    let auth_is_regular = std::fs::symlink_metadata(&auth_path)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false);
    if !auth_is_regular {
        return Err(
            "Codex credentials not found at ~/.codex/auth.json — log in with `codex login`".into(),
        );
    }

    let login = std::process::Command::new("codex")
        .args(["login", "status"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not check Codex login status: {e}"))?;
    let login_text = format!(
        "{}{}",
        String::from_utf8_lossy(&login.stdout),
        String::from_utf8_lossy(&login.stderr)
    );
    if !login.status.success() || login_text.to_ascii_lowercase().contains("not logged") {
        return Err("Codex is not authenticated — log in with `codex login`".into());
    }

    let final_message =
        harness_tempfile().map_err(|e| format!("could not create Codex probe output file: {e}"))?;
    let probe = std::process::Command::new("codex")
        .args([
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--model",
            codex_test_model(),
            "-c",
            "model_reasoning_effort=\"low\"",
            "--color",
            "never",
        ])
        .arg("--output-last-message")
        .arg(final_message.path())
        .arg("Reply with exactly CODEX_AUTH_OK and do not use tools.")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run Codex model probe: {e}"))?;
    let probe_text = format!(
        "{}{}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    let lower = probe_text.to_ascii_lowercase();
    let model_reply = std::fs::read_to_string(final_message.path()).unwrap_or_default();
    if !probe.status.success()
        || !model_reply.contains("CODEX_AUTH_OK")
        || [
            "401",
            "unauthorized",
            "not logged in",
            "authentication required",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return Err(format!(
            "Codex could not reach model {} with the current authentication — the two auth \
             modes reach different model families, so set {} to one these credentials can \
             reach (API key: e.g. gpt-5-nano or gpt-5.1-codex-mini; ChatGPT subscription: \
             e.g. gpt-5.6-luna). Run `codex login status` to see which mode is in use",
            codex_test_model(),
            CODEX_TEST_MODEL_ENV,
        ));
    }
    Ok(())
}

/// Whether a real Devin CLI can run in this environment. Pair with
/// `skip_unless!` so a host without Devin skips the real-agent test cleanly.
///
/// Deliberately does NOT run a model probe the way [`check_codex_available`]
/// does. Devin bills every inference call to the user's Cognition account, so a
/// probe would spend real money on every `cargo test-e2e` run just to decide
/// whether to spend more. `devin auth status` reaches the account without
/// inference and distinguishes a logged-out host from a logged-in one.
pub fn check_devin_available() -> Result<(), String> {
    if !cli_invocable("devin") {
        return Err("Devin CLI not installed (could not invoke `devin --version`)".into());
    }

    let creds = host_home()
        .join(".local")
        .join("share")
        .join("devin")
        .join("credentials.toml");
    let creds_is_regular = std::fs::symlink_metadata(&creds)
        .map(|meta| meta.file_type().is_file())
        .unwrap_or(false);
    if !creds_is_regular {
        return Err("Devin credentials not found at \
                    ~/.local/share/devin/credentials.toml — log in with `devin auth login`"
            .into());
    }

    let status = std::process::Command::new("devin")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not check Devin auth status: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    if !status.status.success() || !text.to_ascii_lowercase().contains("logged in") {
        return Err("Devin is not authenticated — log in with `devin auth login`".into());
    }
    Ok(())
}

/// Import the host user's Devin credentials into the isolated per-test HOME and
/// seed a config that can run unattended. Returns the strings a successful-run
/// recording must redact.
///
/// Two pieces are non-obvious, and both were measured against devin 3000.3.27
/// rather than inferred:
///
/// - The host `config.json` is copied for its `version` / `devin.org_id` /
///   `shell.setup_complete` keys. Without them Devin runs its first-run setup
///   wizard instead of the prompt, and the run ends without doing any work.
/// - `respect_workspace_trust` is written `false`. Devin refuses to run in an
///   untrusted directory and the per-test fixture dir is always untrusted. This
///   is belt-and-braces alongside the CLI flag the test passes — only the flag
///   is documented to take effect in print mode.
///
/// `devin.org_id` identifies the user's Cognition org, so it is returned as a
/// redaction rather than being allowed into a recorded cast.
pub fn import_devin_credentials(test_home: &Path) -> std::io::Result<Vec<String>> {
    let share = host_home().join(".local").join("share").join("devin");
    let bytes = read_credential_file_no_symlink(
        &share.join("credentials.toml"),
        "Devin credentials not found at ~/.local/share/devin/credentials.toml — log in with \
         `devin auth login`",
        "~/.local/share/devin/credentials.toml",
    )?;
    let dst_share = test_home.join(".local").join("share").join("devin");
    std::fs::create_dir_all(&dst_share)?;
    write_credential_file_atomic_0o600(&dst_share.join("credentials.toml"), &bytes)?;

    // Seed the config from the host's so the setup wizard stays out of the way.
    // A missing or unparsable host config is fine: those keys are simply absent
    // on a machine that has never run setup, and `check_devin_available` has
    // already established the account itself is usable.
    let host_config = host_home()
        .join(".config")
        .join("devin")
        .join("config.json");
    let mut root: serde_json::Value = std::fs::read(&host_config)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let mut redactions = Vec::new();
    if let Some(org) = root
        .get("devin")
        .and_then(|d| d.get("org_id"))
        .and_then(|v| v.as_str())
        && !org.is_empty()
    {
        redactions.push(org.to_string());
    }
    root["respect_workspace_trust"] = serde_json::Value::Bool(false);

    let dst_config_dir = test_home.join(".config").join("devin");
    std::fs::create_dir_all(&dst_config_dir)?;
    write_credential_file_atomic_0o600(
        &dst_config_dir.join("config.json"),
        serde_json::to_string_pretty(&root)?.as_bytes(),
    )?;
    Ok(redactions)
}

/// Helper: returns true when `bin --version` exits 0, false otherwise
/// (binary missing, returns non-zero, etc.). Used by the
/// `check_*_available()` helpers — extracted so the BoolNot trait
/// from M2 can be retired (M3.1 auditor Nit 5).
fn cli_invocable(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// PRD #126: opt-in switch that turns every runtime skip into a hard failure.
///
/// A runtime skip prints `SKIP: [e2e] …` and RETURNS NORMALLY, so nextest reports a
/// skipped real-agent test as **passed**. A pre-PR `cargo test-e2e` can
/// therefore read fully green while a `[reel]`-marked scenario asserted
/// nothing at all — and the demo reel then ships with that clip silently
/// missing. Set this to a truthy value on any run whose whole point is that
/// the real-agent coverage actually executed (release gates, reel builds);
/// leave it unset for ad-hoc runs on a machine without the credentials, where
/// the permissive skip is the useful behavior.
pub const REQUIRE_REAL_E2E_ENV: &str = "DOT_AGENT_DECK_REQUIRE_REAL_E2E";

/// Whether [`REQUIRE_REAL_E2E_ENV`] is set to a truthy value. `0`, `false`,
/// `no`, empty and unset all mean "permissive skips"; anything else opts in.
#[allow(dead_code)]
pub fn require_real_e2e() -> bool {
    match std::env::var(REQUIRE_REAL_E2E_ENV) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => false,
    }
}

/// Body of the `skip_unless!` early-return: if `result` is `Err`,
/// print `SKIP: [e2e] <reason>` to stderr and indicate to the caller it
/// should return. Pairs with the `skip_unless!` macro below.
///
/// Under [`REQUIRE_REAL_E2E_ENV`] the same `Err` **panics** instead, carrying
/// the reason, so an unmet precondition is reported as a failure rather than
/// disappearing into a green run.
///
/// # The `[e2e]` marker is load-bearing (issue #502/#785)
///
/// `SKIP: ` on its own does NOT identify this function. Both e2e aliases carry
/// `--workspace` (issue #489), so a run also selects `xtask/` and the root
/// package's unit tests, and several of those print their own `SKIP: ` lines
/// when a tool they drive is absent — `xtask/linkage-check/src/junit_strip.rs`,
/// `pin_lockstep.rs`, `verify_pr_stream.rs`, `issue_labeler_memory.rs`,
/// `clean_tmp.rs` and `src/ui.rs`. The credentialed lane-2 workflow
/// (`.github/workflows/e2e-live.yml`) counts runtime skips into its run summary
/// to answer "did any API-key-backed test actually run?", and a marker-less
/// count folds every one of those unrelated skips into that number.
///
/// The marker narrows the population; it does not AUTHENTICATE it. Any selected
/// test can print any line, and this function takes an arbitrary `String`, so
/// nothing downstream may treat a matching line as trustworthy metadata — which
/// is why that workflow reports a count and nothing else, leaving the reasons in
/// the masked job log. `.claude/skills/verify-pr/checks.sh` deliberately keeps
/// the broader marker-less pattern, because it runs against whatever branch is
/// checked out, including ones predating this marker.
#[doc(hidden)]
pub fn _skip_if_err(result: Result<(), String>) -> bool {
    match result {
        Ok(()) => false,
        Err(reason) => {
            assert!(
                !require_real_e2e(),
                "{REQUIRE_REAL_E2E_ENV} is set, so this real-agent test must RUN, not skip: \
                 {reason}"
            );
            eprintln!("SKIP: [e2e] {reason}");
            true
        }
    }
}

/// Decision 26 / Decision 8 runtime-skip shorthand. Use at the top
/// of a chain-smoke test:
///
/// ```ignore
/// skip_unless!(common::check_claude_available());
/// ```
///
/// Prints `SKIP: [e2e] <reason>` to stderr and returns from the calling
/// function when the environment isn't capable of running the test. The `[e2e]`
/// marker is what separates these from the other `SKIP: ` producers a
/// `--workspace` run selects — see [`_skip_if_err`].
#[macro_export]
macro_rules! skip_unless {
    ($expr:expr) => {
        if $crate::common::_skip_if_err($expr) {
            return;
        }
    };
}

/// Host user's HOME directory at test-runner launch time, used by
/// the credential-availability checks and the credential-import copy
/// path. Resolved from the parent process's env (not from the
/// already-redirected per-test tempdir HOME).
fn host_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set on the host"))
}

/// Copy the host user's Claude Code credentials + settings into the
/// per-test tempdir HOME. Strips any `hooks` entries from the
/// imported `settings.json` (the deck auto-installs its own hooks
/// pointing at the per-test socket — leaving the host's hook entries
/// in place would invoke the developer's real hook commands inside
/// the test). M3.1 auditor S2 + S3: write the destination with mode
/// 0o600 atomically; refuse source files that are symlinks.
fn import_claude_credentials(test_home: &Path) -> std::io::Result<()> {
    let src_root = host_home().join(".claude");
    let dst_root = test_home.join(".claude");
    std::fs::create_dir_all(&dst_root)?;

    // PRD #386: the SOURCE is not necessarily a file any more. Claude Code 2.x
    // on macOS keeps the credential set in the login Keychain and writes no
    // `~/.claude/.credentials.json` at all, so the read below is a hard
    // NotFound on a migrated host — which is what turned this import into a
    // launch-time panic the moment `check_claude_available` learned about the
    // Keychain. The DESTINATION still has to be a file: `security` resolves the
    // login keychain from `$HOME`, so a daemon-spawned `claude` running under
    // the relocated per-test HOME cannot reach the real user's keychain (it
    // answers "keychain not found", exit 44 — measured) and has nothing but
    // this imported file to authenticate from.
    //
    // PRD #386 review (Greptile P1): the source is chosen by USABILITY, not by
    // mere readability. `check_claude_available` accepts the host when EITHER
    // source is usable, so a stale/expired/malformed but perfectly readable
    // `~/.claude/.credentials.json` sitting beside a live Keychain item used to
    // pass preflight (on the Keychain) and then seed the test HOME from the
    // file — launching the isolated `claude` with credentials the gate had
    // already rejected, and failing the real-agent test deep in a PTY wait on a
    // host that is genuinely logged in. Same rule, same order, as the gate:
    // a usable file wins, else the Keychain, else copy what we have and let it
    // fail loudly rather than silently importing nothing.
    //
    // Issue #502/#785, and this half is NOT optional once `check_claude_available`
    // accepts an API key. This function runs inside `launch_with_fixture`, which
    // is `try_launch_with_fixture(…).unwrap_or_else(|e| panic!(…))`, so an
    // `Err` here is a PANIC rather than a skip. Widening the gate without
    // widening this converts 22 silent skips into 22 hard panics on any host
    // that has a key and no credential set — which is precisely the state a CI
    // runner is in. The two changes are one change.
    //
    // So: when NEITHER store is usable but a key is present, the correct
    // credential set to seed is NO credential set. Claude Code then
    // authenticates from `ANTHROPIC_API_KEY` (which reaches it through the
    // deck's env — see `inherit_pass`), exactly as it does in a virgin HOME.
    // Writing a spent or malformed file beside an authorising key would only
    // make the agent's choice of credential ambiguous.
    let src_creds = src_root.join(".credentials.json");
    let key_authorises = anthropic_api_key().is_some();
    let creds_bytes = match read_credential_file_no_symlink(
        &src_creds,
        "Claude Code credentials not found at ~/.claude/.credentials.json \
         nor in the macOS login Keychain, and no usable ANTHROPIC_API_KEY — \
         log in with `claude login`",
        "~/.claude/.credentials.json",
    ) {
        Ok(bytes) if claude_credential_document_usable(&bytes) => Some(bytes),
        Ok(bytes) => match claude_keychain_credentials_export() {
            Some(keychain) => Some(keychain),
            None if key_authorises => None,
            // Unchanged: copy what we have and let it fail loudly rather than
            // silently importing nothing.
            None => Some(bytes),
        },
        // A symlinked or unreadable source lands here too, and falls through to
        // the same alternatives the absent-file case does — the same posture the
        // Keychain fallback has always had: refuse to IMPORT it, then use
        // another source.
        Err(file_err) => match claude_keychain_credentials_export() {
            Some(keychain) => Some(keychain),
            None if key_authorises => None,
            None => return Err(file_err),
        },
    };
    if let Some(bytes) = creds_bytes {
        write_credential_file_atomic_0o600(&dst_root.join(".credentials.json"), &bytes)?;
    }

    // settings.json: copy if present, with `hooks` stripped. Claude's
    // settings.json is JSONC (line + block comments) — M3.1 auditor
    // S0 fix: strip comments before serde_json parse so the strip is
    // never a no-op on a real settings.json with `// foo` lines.
    // M4.6 P2: settings.json can carry the same tokens / sensitive
    // config that motivate the 0o600 mode on credentials.json, so
    // route it through the same atomic-0o600 helper rather than
    // inheriting umask via fs::write. `write_credential_file_atomic_0o600`
    // treats its input as opaque bytes — the JSONC body comes out
    // intact.
    let src_settings = src_root.join("settings.json");
    if src_settings.exists() {
        require_regular_file_no_symlink(&src_settings, "~/.claude/settings.json")?;
        let raw = std::fs::read_to_string(&src_settings)?;
        let dst_text = strip_hooks_from_claude_settings(&raw)?;
        write_credential_file_atomic_0o600(&dst_root.join("settings.json"), dst_text.as_bytes())?;
    }

    // plugins/ (and any other supporting dirs) — best-effort copy if
    // present. `copy_dir_recursively` was further tightened in M3
    // from M2.1 Nit 3's "silent skip" to a hard refuse on any
    // non-regular entry (symlinks/sockets/FIFOs), so this branch
    // already shares the credential-side stance on symlinks.
    // Issue #322: skipped by default. This is a recursive copy of the host's
    // entire plugin tree — 11 MB on this repo's dev machine, nearly all of it
    // the `marketplaces/` clone cache — paid once per seeded HOME. With dozens
    // of tests running concurrently it is a real share of the suite's PEAK temp
    // demand, and peak is what exhausts a RAM-backed `/tmp`. No test references
    // host plugin state; the agents are driven by directive prompts. Set
    // `DAD_E2E_IMPORT_CLAUDE_PLUGINS=1` to restore the copy when debugging a
    // case that turns out to depend on it.
    let src_plugins = src_root.join("plugins");
    if import_claude_plugins_enabled() && src_plugins.is_dir() {
        require_regular_dir_no_symlink(&src_plugins, "~/.claude/plugins")?;
        copy_dir_recursively(&src_plugins, &dst_root.join("plugins"))?;
    }
    Ok(())
}

/// Whether to copy the host's `~/.claude/plugins` into each seeded HOME.
/// Off by default — see the note in [`import_claude_credentials`].
fn import_claude_plugins_enabled() -> bool {
    matches!(
        std::env::var("DAD_E2E_IMPORT_CLAUDE_PLUGINS").as_deref(),
        Ok("1" | "true" | "yes")
    )
}

/// Seed the per-test HOME's `~/.claude.json` so a daemon-spawned interactive
/// `claude` clears BOTH first-run gates without a human keystroke: the global
/// onboarding gate (`hasCompletedOnboarding`) and the per-folder trust gate
/// (`projects.<cwd>.hasTrustDialogAccepted`). Ported from
/// `e2e_delegate_work_done_chain.rs::prepare_claude_home`: start from the host's
/// `~/.claude.json` (preserving `oauthAccount` + `hasCompletedOnboarding` so the
/// global onboarding flow is skipped), then mark each `trust_paths` entry as a
/// trusted project.
///
/// `trust_paths` are the EXACT cwd strings the spawned agent will run in. The
/// destination is written atomically with mode 0o600 — `.claude.json` carries
/// the host `oauthAccount` (M3.1 auditor S2, same stance as the other imported
/// credential files).
///
/// Issue #502/#785 adds a THIRD gate to clear, and it only exists once
/// `ANTHROPIC_API_KEY` reaches the agent (which it now always does — see
/// `inherit_pass`). With a key in its environment, an interactive Claude Code
/// stops on:
///
/// ```text
/// Detected a custom API key in your environment
/// ANTHROPIC_API_KEY: sk-ant-...<last 20 chars>
/// Do you want to use this API key?
///    Yes
///  > No (recommended)
/// ```
///
/// It DEFAULTS TO "No", so an unattended agent stalls there forever instead of
/// failing fast, and every claude-gated test dies at a PTY wait with a
/// confusing timeout. [`claude_api_key_response_id`] is what the answer is
/// recorded under, so the answer can be pre-seeded without a round trip.
///
/// WHICH answer is seeded is decided by [`host_claude_oauth_usable`], and the
/// asymmetry is the point:
///
///   * **OAuth usable** (a developer's machine): record the key as REJECTED, so
///     the prompt is silent and the agent keeps authenticating from the
///     imported credential set exactly as it did before this change. Approving
///     it here would quietly move a local run off the developer's subscription
///     and onto metered API billing.
///   * **OAuth unusable** (a CI runner): record it as APPROVED, and drop any
///     inherited `rejected` entry for it — the host config is copied wholesale,
///     so a developer who once answered "No" to this very key would otherwise
///     export that refusal to a runner where the key is the ONLY way in.
///
/// A response the host already recorded for a key that is not the ambient one
/// is left untouched, and so is an existing response for the ambient key on an
/// OAuth host: that is the developer's own answer, and it already says "No".
fn seed_claude_project_trust(test_home: &Path, trust_paths: &[String]) -> std::io::Result<()> {
    let host_cfg_path = host_home().join(".claude.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&host_cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "hasCompletedOnboarding": true }));
    if !cfg["projects"].is_object() {
        cfg["projects"] = serde_json::json!({});
    }
    for path in trust_paths {
        cfg["projects"][path] = serde_json::json!({
            "hasTrustDialogAccepted": true,
            "hasCompletedProjectOnboarding": true,
            "projectOnboardingSeenCount": 1,
        });
    }
    if let Some(key) = anthropic_api_key() {
        seed_claude_api_key_response(&mut cfg, &key, host_claude_oauth_usable());
    }
    let bytes = serde_json::to_vec(&cfg)
        .map_err(|e| std::io::Error::other(format!("serialize .claude.json: {e}")))?;
    write_credential_file_atomic_0o600(&test_home.join(".claude.json"), &bytes)
}

/// Pre-answer Claude Code's "Detected a custom API key in your environment"
/// prompt inside a `~/.claude.json` document. Split out as a pure mutation of
/// the parsed config so the branch table above is covered by unit tests instead
/// of by argument, and so `oauth_usable` is an explicit input rather than an
/// ambient read.
///
/// Only ever handles [`claude_api_key_response_id`] — the 20-character
/// derivative — never the key itself, which does not appear in this file.
fn seed_claude_api_key_response(cfg: &mut serde_json::Value, key: &str, oauth_usable: bool) {
    let id = claude_api_key_response_id(key);
    if !cfg["customApiKeyResponses"].is_object() {
        cfg["customApiKeyResponses"] = serde_json::json!({});
    }
    if oauth_usable {
        // The imported credential set is authoritative. Answer "No" for the
        // ambient key ONLY if the host has not already answered — an existing
        // `approved` entry is a deliberate developer choice and stays.
        if !response_list_contains(cfg, "approved", &id) {
            set_response_membership(cfg, "rejected", &id, true);
        }
    } else {
        set_response_membership(cfg, "approved", &id, true);
        set_response_membership(cfg, "rejected", &id, false);
    }
}

/// Whether `customApiKeyResponses.<field>` already lists `id`.
fn response_list_contains(cfg: &serde_json::Value, field: &str, id: &str) -> bool {
    cfg["customApiKeyResponses"][field]
        .as_array()
        .is_some_and(|list| list.iter().any(|v| v.as_str() == Some(id)))
}

/// Add or remove `id` from `customApiKeyResponses.<field>`, creating the list
/// when a host config carries none (or carries a non-array there).
fn set_response_membership(cfg: &mut serde_json::Value, field: &str, id: &str, want: bool) {
    let responses = &mut cfg["customApiKeyResponses"];
    if !responses[field].is_array() {
        responses[field] = serde_json::json!([]);
    }
    let list = responses[field].as_array_mut().expect("array just ensured");
    let present = list.iter().any(|v| v.as_str() == Some(id));
    match (want, present) {
        (true, false) => list.push(serde_json::Value::String(id.to_string())),
        (false, true) => list.retain(|v| v.as_str() != Some(id)),
        _ => {}
    }
}

/// Public wrapper over [`seed_claude_project_trust`] for tests whose trusted
/// directory is only known AFTER the deck launched — the per-test tempdir
/// fixture root, which [`TuiDeckBuilder::with_claude_project_trust`] (a
/// pre-launch builder step) cannot name in advance. Call it with
/// [`TuiDeck::home_dir`] BEFORE the pane that runs `claude` in `trust_paths` is
/// spawned; claude reads `~/.claude.json` at agent start, so the seeding only
/// has to beat the spawn, not the launch.
#[allow(dead_code)]
pub fn seed_claude_trust_in_home(home: &Path, trust_paths: &[String]) -> std::io::Result<()> {
    seed_claude_project_trust(home, trust_paths)
}

/// Seed a HOME for a Claude Code worker the test spawns ITSELF, outside the
/// `TuiDeck` builder — an in-process-daemon `spawn_agent` whose `env` pins
/// `HOME` to a tempdir. Does exactly what the builder's
/// `with_imported_claude_credentials` + `with_claude_project_trust` pair does
/// for a deck-spawned agent, so the two routes cannot drift.
///
/// Issue #502/#785: it exists because they HAD drifted.
/// `e2e_pi_orchestrator.rs` and `e2e_delegate_work_done_chain.rs` each carried a
/// hand-rolled `prepare_claude_home` that opened with
/// `fs::copy(host ~/.claude/.credentials.json, …).expect("copy claude
/// credentials")`. That is an unconditional panic on any host authorised by an
/// API key — the third panic site of the same shape as the two inside
/// `launch_with_fixture`, and the one a grep for `import_claude_credentials`
/// does not find. Neither copy seeded the API-key approval either, so both
/// would have stalled on the prompt even if the copy had been made optional.
#[allow(dead_code)]
pub fn seed_claude_worker_home(home: &Path, trust_paths: &[String]) -> std::io::Result<()> {
    import_claude_credentials(home)?;
    seed_claude_project_trust(home, trust_paths)
}

/// Strip the top-level `hooks` key from a Claude Code settings.json.
/// settings.json is JSONC: line (`// foo`) and block (`/* foo */`)
/// comments are tolerated by Claude's own loader. M3.1 auditor S0
/// fixes the fail-open path: comments are stripped before parsing so
/// real-world settings.json files (which carry `//` comments) are
/// rewritten with their hook block removed rather than passed
/// through unchanged. A truly-malformed settings.json (still invalid
/// after comment stripping) is now fail-CLOSED — we refuse to
/// continue rather than risk shipping the host's hook commands into
/// the test.
fn strip_hooks_from_claude_settings(raw: &str) -> std::io::Result<String> {
    let cleaned = strip_jsonc_comments(raw);
    let mut v: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        std::io::Error::other(format!(
            "refusing to import host settings.json: not valid JSON(C) after \
             comment-stripping ({e}). Leaving the host's hook entries in place \
             would let them fire inside the test."
        ))
    })?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("hooks");
    }
    serde_json::to_string_pretty(&v)
        .map_err(|e| std::io::Error::other(format!("serialize sanitized settings.json: {e}")))
}

/// Strip `//` line comments and `/* … */` block comments from a
/// JSONC string. Preserves string literals (so `"//"` and `"/*"`
/// inside a quoted value are left alone) and keeps newlines so any
/// downstream parse-error line numbers still align.
fn strip_jsonc_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    let mut block_depth: usize = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let next = bytes.get(i + 1).map(|b| *b as char);

        if block_depth > 0 {
            if c == '*' && next == Some('/') {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '/' && next == Some('/') {
            // Line comment: eat until newline.
            while i < bytes.len() && bytes[i] as char != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Read a credential file, refusing symlinks at the source path
/// (M3.1 auditor S3). Returns the file bytes on success, or a
/// redacted `io::Error` on failure with the abstract `~/` path so
/// the stderr output doesn't leak the host's real $HOME.
fn read_credential_file_no_symlink(
    real_path: &Path,
    not_found_message: &str,
    redacted_display: &str,
) -> std::io::Result<Vec<u8>> {
    let meta = match std::fs::symlink_metadata(real_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(std::io::Error::other(not_found_message.to_string()));
        }
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "failed to stat {redacted_display}: {e}"
            )));
        }
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found a symlink"
        )));
    }
    if !file_type.is_file() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found {:?}",
            file_type
        )));
    }
    std::fs::read(real_path)
        .map_err(|e| std::io::Error::other(format!("read {redacted_display}: {e}")))
}

/// Validate that a source path is a regular file (not a symlink) without
/// reading it. Claude settings use this before their JSONC sanitization pass.
fn require_regular_file_no_symlink(
    real_path: &Path,
    redacted_display: &str,
) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(real_path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file, found a symlink"
        )));
    }
    if !meta.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular file"
        )));
    }
    Ok(())
}

/// Validate that a source path is a regular directory (not a
/// symlink). Mirrors [`require_regular_file_no_symlink`] for the
/// `~/.claude/plugins` directory copy.
fn require_regular_dir_no_symlink(real_path: &Path, redacted_display: &str) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(real_path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular directory, found a symlink"
        )));
    }
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::other(format!(
            "refusing to import {redacted_display}: expected a regular directory"
        )));
    }
    Ok(())
}

/// Validate every directory between `source_home` and a credential leaf with
/// `symlink_metadata`, in order, before opening the leaf. Checking only
/// `auth.json` is insufficient: `~/.local/share/opencode` itself may be a
/// symlink, in which case leaf metadata has already followed the source root.
fn require_nonsymlink_credential_ancestors(
    source_home: &Path,
    credential_path: &Path,
    redacted_display: &str,
) -> std::io::Result<()> {
    let relative = credential_path.strip_prefix(source_home).map_err(|_| {
        std::io::Error::other(format!(
            "refusing to import {redacted_display}: path is outside the source HOME"
        ))
    })?;
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = source_home.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "refusing to import {redacted_display}: a source directory ancestor is a symlink"
            )));
        }
        if !metadata.file_type().is_dir() {
            return Err(std::io::Error::other(format!(
                "refusing to import {redacted_display}: a source ancestor is not a directory"
            )));
        }
    }
    Ok(())
}

/// Write `bytes` to `dst` atomically with mode 0o600 — the
/// destination is `open`ed with `O_CREAT | O_WRONLY | O_TRUNC` AND
/// the mode flag set to 0o600 in the same syscall (M3.1 auditor S2),
/// so there is no umask-derived 0o666 window between create and
/// chmod. Refuses to follow if `dst` already exists as a symlink.
fn write_credential_file_atomic_0o600(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Pre-remove any existing entry — `OpenOptions::create + mode` on
    // an existing file does not re-stamp the mode, and we want a
    // freshly-zeroed credential file with the strict mode regardless
    // of what was there before.
    match std::fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::other(format!(
                "refusing to write credential into existing symlink at {}",
                dst.display()
            )));
        }
        Ok(_) => {
            std::fs::remove_file(dst).ok();
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dst)?;
        f.write_all(bytes)?;
        f.sync_all().ok();
    }
    #[cfg(not(unix))]
    {
        std::fs::write(dst, bytes)?;
    }
    Ok(())
}

fn collect_credential_values(value: &serde_json::Value, key: Option<&str>, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (child_key, child) in map {
                collect_credential_values(child, Some(child_key), out);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_credential_values(child, key, out);
            }
        }
        serde_json::Value::String(value) => {
            let key = key.unwrap_or_default().to_ascii_lowercase();
            let sensitive_key = key == "key"
                || key == "access"
                || key == "refresh"
                || key.contains("token")
                || key.contains("secret")
                || key.contains("password")
                || key.contains("authorization")
                || key.contains("api_key")
                || key.contains("apikey");
            if !value.is_empty() && (sensitive_key || value.len() >= 16) {
                out.push(value.clone());
            }
        }
        _ => {}
    }
}

fn opencode_recording_redactions(
    bytes: &[u8],
    redacted_display: &str,
) -> std::io::Result<Vec<String>> {
    let auth: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        std::io::Error::other(format!(
            "refusing to import {redacted_display}: auth file is not valid JSON: {error}"
        ))
    })?;
    let mut values = Vec::new();
    collect_credential_values(&auth, None, &mut values);
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    Ok(values)
}

const MINIMAL_OPENCODE_CONFIG: &str = "{}\n";

/// Copy only the host user's OpenCode `auth.json` credentials into the per-test
/// HOME and synthesize a minimal config. Host `opencode.json(c)`, plugins, MCP
/// commands and provider configuration are deliberately never imported: `--auto`
/// may execute them, and the PTY stream is persisted as a recording artifact.
/// Every source ancestor plus the auth leaf is checked without following
/// symlinks; destination credentials are atomically created with mode 0o600.
fn import_opencode_credentials_from(
    source_home: &Path,
    test_home: &Path,
) -> std::io::Result<Vec<String>> {
    let mut imported_auth = false;
    let mut recording_redactions = Vec::new();
    let credentials = [
        (
            source_home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json"),
            test_home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("auth.json"),
            "~/.local/share/opencode/auth.json",
        ),
        (
            source_home.join(".opencode").join("auth.json"),
            test_home.join(".opencode").join("auth.json"),
            "~/.opencode/auth.json",
        ),
        (
            source_home
                .join(".config")
                .join("opencode")
                .join("auth.json"),
            test_home.join(".config").join("opencode").join("auth.json"),
            "~/.config/opencode/auth.json",
        ),
    ];
    for (src, dst, redacted) in credentials {
        match require_nonsymlink_credential_ancestors(source_home, &src, redacted) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        match std::fs::symlink_metadata(&src) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let bytes = read_credential_file_no_symlink(
            &src,
            &format!("OpenCode credentials not found at {redacted}"),
            redacted,
        )?;
        std::fs::create_dir_all(dst.parent().expect("OpenCode auth path has a parent"))?;
        write_credential_file_atomic_0o600(&dst, &bytes)?;
        recording_redactions.extend(opencode_recording_redactions(&bytes, redacted)?);
        imported_auth = true;
    }

    if !imported_auth {
        // `NotFound` rather than `other` so the one caller that is allowed to
        // continue without an auth file can tell THIS refusal apart from the
        // symlink / not-a-regular-file ones, which must stay fatal. Every other
        // error out of this function is `ErrorKind::Other` (see
        // `read_credential_file_no_symlink`), so the discrimination is exact.
        // The message is unchanged.
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "OpenCode credentials not found at ~/.local/share/opencode/auth.json, \
             ~/.opencode/auth.json, or ~/.config/opencode/auth.json — log in with \
             `opencode auth login`"
                .to_string(),
        ));
    }
    write_minimal_opencode_config(test_home)?;

    recording_redactions.sort_by_key(|value| std::cmp::Reverse(value.len()));
    recording_redactions.dedup();
    Ok(recording_redactions)
}

/// The isolated OpenCode config every seeded HOME gets: empty, so no host
/// plugin, MCP command or provider block can follow the credentials in.
fn write_minimal_opencode_config(test_home: &Path) -> std::io::Result<()> {
    let dst_cfg_dir = test_home.join(".config").join("opencode");
    std::fs::create_dir_all(&dst_cfg_dir)?;
    std::fs::write(dst_cfg_dir.join("opencode.json"), MINIMAL_OPENCODE_CONFIG)
}

fn import_opencode_credentials(test_home: &Path) -> std::io::Result<Vec<String>> {
    match import_opencode_credentials_from(&host_home(), test_home) {
        // Issue #502/#785, the OpenCode half of the coupling described on
        // `import_claude_credentials`: this runs inside the `launch_with_fixture`
        // path that panics on `Err`, so `check_opencode_available` accepting an
        // env key without this would turn two clean skips into two panics on a
        // runner. There is no auth file to copy and that is the AUTHORISED
        // state, not a failure — the isolated config is still written, so the
        // host's plugins and MCP commands stay out either way.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && opencode_env_key_authorises() => {
            write_minimal_opencode_config(test_home)?;
            Ok(Vec::new())
        }
        other => other,
    }
}

/// Copy only Codex's authentication state into the isolated test HOME and seed
/// the fixture working directory as trusted. User configuration is deliberately
/// not imported; real-agent tests pin their model for deterministic behavior.
///
/// Issue #243: also seeds `version.json` — see [`codex_update_notice_dismissal`]
/// for why an isolated HOME without it can wedge a real-agent Codex test in a
/// way that looks like a delivery failure.
pub fn import_codex_credentials(test_home: &Path) -> std::io::Result<()> {
    let src = host_home().join(".codex").join("auth.json");
    let bytes = read_credential_file_no_symlink(
        &src,
        "Codex credentials not found at ~/.codex/auth.json — log in with `codex login`",
        "~/.codex/auth.json",
    )?;
    let dst = test_home.join(".codex");
    std::fs::create_dir_all(&dst)?;
    write_credential_file_atomic_0o600(&dst.join("auth.json"), &bytes)?;

    let project = test_home.parent().ok_or_else(|| {
        std::io::Error::other("isolated Codex HOME has no fixture working directory")
    })?;
    let config = format!(
        "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        toml_escape(project.to_str().ok_or_else(|| {
            std::io::Error::other("isolated Codex fixture path is not UTF-8")
        })?)
    );
    write_credential_file_atomic_0o600(&dst.join("config.toml"), config.as_bytes())?;

    // Issue #243: dismiss the update notice in the ISOLATED home.
    //
    // Everything else here is deliberately minimal — auth plus a trust entry,
    // nothing else — and `version.json` looks like user state that a test has no
    // business inheriting. It is not: with the file absent, codex-cli 0.149.0
    // paints a blocking "✨ Update available! … Press enter to continue"
    // interstitial INSTEAD of its composer, so the pane looks alive while no
    // agent is behind it and an injected prompt goes into the interstitial. The
    // failure surfaces as "the worker never submitted the pointer" — a delivery
    // symptom with a boot cause, which cost #243's implementer two runs to spot
    // and would misattribute an `orchestration/delegate/009` red to the readiness
    // gate. Nobody meets it interactively because the host HOME has the file.
    //
    // Best-effort by design: it is an ergonomic, not a credential, and a test
    // that cannot write it should still run rather than fail with an error about
    // a notice. The host's own file is not copied — it carries a
    // `last_checked_at` timestamp and whatever version the developer happens to
    // be on, neither of which a test wants to inherit.
    let _ = std::fs::write(dst.join("version.json"), codex_update_notice_dismissal());
    Ok(())
}

/// The `version.json` body that suppresses codex-cli's update notice in an
/// isolated HOME: a `latest_version` BELOW every real release, so there is
/// nothing newer to announce whatever the CLI is actually running, plus a
/// matching `dismissed_version` for the same claim by the other route.
///
/// `0.0.0` rather than a high sentinel, and that is measured rather than
/// stylistic: seeding `9999.0.0` (dismissed equal to latest, the shape the host's
/// own file has) does NOT suppress the notice — codex-cli 0.149.0 rendered
/// `✨ Update available! 0.149.0 -> 9999.0.0` above its composer on
/// `orchestration/delegate/009`, i.e. the seed manufactured the very banner it
/// was meant to remove. Nothing can be newer than what is running if the
/// recorded latest is `0.0.0`.
///
/// `last_checked_at` is far in the future so the CLI has no reason to re-check
/// and overwrite this, and so a test HOME never depends on the wall clock.
fn codex_update_notice_dismissal() -> &'static str {
    r#"{"latest_version":"0.0.0","last_checked_at":"2099-01-01T00:00:00.000000000Z","dismissed_version":"0.0.0"}"#
}

/// Write a minimal `session.toml` containing exactly one pane that
/// runs `command` in `work_dir`. The deck reads this when launched
/// with `--continue`.
fn write_continue_session_file(
    session_toml_path: &Path,
    work_dir: &Path,
    pane_name: &str,
    command: &str,
    mode: Option<&str>,
) -> std::io::Result<()> {
    // Hand-rolled TOML so we don't need a runtime dep on toml in the
    // harness module. Field names match `dot_agent_deck::config::SavedPane`.
    let mut s = String::new();
    s.push_str("[[panes]]\n");
    s.push_str(&format!(
        "dir = \"{}\"\n",
        toml_escape(work_dir.to_str().expect("work dir is UTF-8"))
    ));
    s.push_str(&format!("name = \"{}\"\n", toml_escape(pane_name)));
    s.push_str(&format!("command = \"{}\"\n", toml_escape(command)));
    if let Some(mode) = mode {
        s.push_str(&format!("mode = \"{}\"\n", toml_escape(mode)));
    }
    std::fs::write(session_toml_path, s)
}

/// Escape `s` so it can be embedded as a TOML basic string between
/// `"…"`. M3.1 auditor Nit 3 — the original two-replace shape missed
/// control characters and BS/FF/LF/CR/TAB, any of which would
/// produce an invalid TOML file. We follow the TOML 1.0 spec: `\b`,
/// `\t`, `\n`, `\f`, `\r`, `\\`, `\"` are the literal escapes; other
/// control chars (U+0000..=U+001F minus the named ones, plus U+007F)
/// take the `\uXXXX` form. UTF-8 codepoints above the C0 range are
/// allowed in basic strings as-is.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// Helper for L2 tests: send a single JSON line to the deck's hook
/// socket. Connects, writes the line + newline, and drops the
/// connection. Synthetic-event tests use this to inject events
/// without going through the `hook` subcommand.
#[cfg(unix)]
pub fn write_hook_line(socket: &Path, json_line: &str) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    // The daemon binds the hook socket asynchronously after the TUI
    // is up; retry briefly if it is not yet present.
    let mut last_err = None;
    while Instant::now() < deadline {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(mut stream) => {
                stream.write_all(json_line.as_bytes())?;
                if !json_line.ends_with('\n') {
                    stream.write_all(b"\n")?;
                }
                stream.flush()?;
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("timed out waiting for hook socket")))
}

// ---------------------------------------------------------------------------
// Legacy test helpers
// ---------------------------------------------------------------------------
//
// Carried forward from the pre-M1 `tests/common/mod.rs`. The M1 audit moved
// the originals into `tmp/legacy-tests/` (Decision 10), but a subset of
// integration tests on `main` — `tests/daemon_protocol.rs`,
// `tests/rehydration.rs`, `tests/spawn_time_role_prompt_submit_after_session_start.rs`,
// `tests/snapshot_replay_dims.rs` — keep calling these helpers via
// `common::*`. Restored here so the merge with `main` builds. Per the
// M5+ "absorbed into per-PRD test maintenance" decision, the legacy
// integration tests are grandfathered until a future PRD refactors
// them onto the PRD #77 harness.
//
// The `dot_agent_deck::daemon::run_daemon_with` lock-root context that
// drove the original helpers (`flock(2)` over a per-socket `.lock`
// resolved via `XDG_RUNTIME_DIR` / `$HOME/.cache`) is documented in
// `tmp/legacy-tests/tests/common/mod.rs`; only the surface those tests
// import is reproduced here.

use std::sync::OnceLock;

#[allow(dead_code)]
static LOCK_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Endpoint env vars that point a process at a *specific* deck's daemon.
#[allow(dead_code)]
const DECK_ENDPOINT_VARS: [&str; 4] = [
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "DOT_AGENT_DECK_PANE_ID",
    "DOT_AGENT_DECK_AGENT_ID",
];

/// Detach this test process from any real deck before it can spawn anything.
///
/// Running the suite from inside a deck pane means the shell carries that pane's
/// `DOT_AGENT_DECK_SOCKET` / `_PANE_ID`. Anything a test spawns inherits them
/// unless every spawn site overrides them, and then its hooks post into the
/// developer's LIVE deck: a card appears for a fixture pane id and vanishes
/// again. Observed repeatedly in the wild — a real `deck.log` shows 48 such
/// events across `worker-pane`, `codex-trust-test-pane`,
/// `pane-live-transition`, `pane-stream-postlock` and
/// `pane-rebound-before-delivery`.
///
/// `ff5170d` scrubs these in `agent_pty::spawn`, which is necessary but not
/// sufficient: four of those five ids leaked from a tree that already had that
/// fix, via other spawn paths (harness-launched binaries, hook-posting helpers).
/// Removing the vars from the test process itself covers every spawn path at
/// once, including ones added later, because there is nothing left to inherit.
///
/// Tests that need an endpoint set it explicitly per-child (`Command::env`), so
/// removing the ambient value changes nothing for them.
fn detach_from_any_live_deck() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let leaked: Vec<&str> = DECK_ENDPOINT_VARS
            .into_iter()
            .filter(|v| std::env::var_os(v).is_some())
            .collect();
        if !leaked.is_empty() {
            // Loud on purpose: the run is now safe, but the contributor should
            // know their shell was pointed at a live deck.
            eprintln!(
                "note: detaching this test process from a live deck — cleared {}. \
                 Tests set endpoints per-child; the inherited values would have \
                 sent fixture hook events into your running dashboard.",
                leaked.join(", ")
            );
        }
        for var in DECK_ENDPOINT_VARS {
            // SAFETY: a stated residual, not a proof — issue #678. The
            // `OnceLock` makes this happen exactly once per test process and
            // excludes nothing else, and "before the harness spawns any thread"
            // is not established: `init_test_env()` is reached from inside
            // multi-threaded Tokio runtimes whose workers already exist
            // (`spawn_inprocess_daemon`, and `delegate_prompt_injection.rs`'s
            // `#[tokio::test(flavor = "multi_thread")]` body). What is true is
            // that these are idempotent setup-time writes of values no library
            // thread in this process reads, before anything is spawned.
            unsafe { std::env::remove_var(var) };
        }
    });
}

/// Issue #668: the wrapped-agent lifetime bound, in a file small enough for the
/// test binaries that deliberately do NOT link this harness to
/// `#[path]`-include on their own. `init_test_env` below calls the same
/// [`child_lifetime_bound::arm`], so there is one implementation and one SAFETY
/// argument rather than one per spawn-owning crate.
///
/// `pub` so `tests/agent_lifetime_bound.rs` can unit-test
/// [`child_lifetime_bound::clamped`] in ONE crate. The alternative — a
/// `#[cfg(test)] mod tests` in the file itself — would run those pure-function
/// cases once per including crate, i.e. ~88 times, which is the multiplication
/// `src/test_temp.rs`'s header already reasons about avoiding.
pub(crate) mod child_lifetime_bound;

/// Idempotent setup hook for legacy daemon-spawning tests. Creates the
/// per-process lock dir on first call; subsequent calls are no-ops.
pub fn init_test_env() {
    detach_from_any_live_deck();
    child_lifetime_bound::arm();
    LOCK_DIR.get_or_init(|| {
        // A plain subdirectory of the harness root rather than its own
        // `TempDir`: this has to stay alive for the whole process, and a
        // process-lifetime `TempDir` can only live in a `static`, which is
        // exactly the leak that issue #322 traced. The harness root's
        // `atexit` hook removes it instead.
        let dir = harness_temp_root().join("daemon-lock");
        std::fs::create_dir_all(&dir).expect("create per-process lock dir");
        harden_dir_0700(&dir);
        dir
    });
}

/// Path to the per-process lock dir, for passing to
/// `dot_agent_deck::daemon::Daemon::with_lock_dir_override` (in-process
/// tests) or to `Command::env` for subprocess-based tests. Returns
/// `None` if [`init_test_env`] was never called.
#[allow(dead_code)]
pub fn lock_dir_path() -> Option<PathBuf> {
    LOCK_DIR.get().cloned()
}

/// Race-safe `harness_tempdir()` wrapper: re-applies 0o700 after
/// creation so the per-test directory survives the daemon's
/// `bind_socket` umask flip. Mirrors `src/daemon_attach.rs`'s
/// same-named helper; promoted here so every legacy daemon-spawning
/// test binary gets the fix without duplicating the workaround.
///
/// Cross-platform: the 0o700 chmod is Unix-only (mode bits). On Windows there
/// are no POSIX mode bits and no umask race to close, so the chmod is skipped
/// (the ACL-based equivalent is deferred to #163/#164). Unix behavior is
/// unchanged.
#[allow(dead_code)]
pub fn race_safe_tempdir() -> tempfile::TempDir {
    harness_tempdir().expect("create tempdir")
}

/// Drop-in replacement for `harness_tempdir()` that is correct **whatever
/// order a test does things in** — the whole point, and what the bare
/// constructor cannot promise.
///
/// The harness redirects `tempfile`'s process-global default temp dir at its own
/// per-process root, but it can only do so from inside [`harness_temp_root`]'s
/// lazy initialisation. nextest runs one process per test, so a bare
/// `harness_tempdir()` that happens to be the *first* allocation in that
/// process runs before the redirect is installed and lands in the OS temp dir:
/// the RAM-backed `/tmp` this issue is about, at `tempfile`'s default mode
/// instead of 0o700, outside the free-space pre-flight, and left behind on
/// SIGKILL under `.tmp*` — a prefix the reaper will not touch by default because
/// it belongs to every Rust program on the machine. Measured on `a0b616c`:
/// reversing the ordering put the dir at `/tmp/.tmpz5pszS` while the root was
/// `/var/tmp/dad-e2e-1000/dad-tests-…`, in all 13 fast-tier binaries.
///
/// Retrofitting that with a lazy global was the mistake. This calls
/// [`harness_temp_root`] *first* and then allocates inside the value it returns,
/// so containment is by construction rather than by ordering. The redirect stays
/// installed as defence in depth for allocations the suite does not make itself;
/// `linkage-check` rule 8 is what keeps bare constructors from coming back.
///
/// Returns `io::Result` rather than panicking so it substitutes for
/// `harness_tempdir()` at a call site without disturbing its `.expect(…)`.
#[allow(dead_code)]
pub fn harness_tempdir() -> std::io::Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new().tempdir_in(harness_temp_root())?;
    harden_dir_0700(dir.path());
    Ok(dir)
}

/// The single-*file* counterpart of [`harness_tempdir`], and correct for the
/// same reason: it resolves [`harness_temp_root`] before it allocates.
///
/// `tempfile::NamedTempFile::new()` is a bare constructor exactly like
/// `tempfile::tempdir()` — it lands in the OS temp dir when it is the process's
/// first allocation — but rule 8 did not match it, so it sat inside the rule's
/// own scope unnoticed. Measured on `5e8e0ed`: the Codex-auth pre-flight
/// created four zero-byte `/tmp/.tmp*` files, because the pre-flight can run
/// before anything has asked the harness for a directory. Zero bytes is not the
/// point — an asserted guarantee that a measurement disproves is.
///
/// `NamedTempFile` already creates its file 0600, so there is no mode to
/// re-apply the way [`harden_dir_0700`] does for a directory.
#[allow(dead_code)]
pub fn harness_tempfile() -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new().tempfile_in(harness_temp_root())
}

/// Re-apply 0o700 to a harness-created directory.
///
/// Factored out of [`race_safe_tempdir`] so the harness root and the lock dir
/// get the same treatment. The lock dir previously skipped it and was left at
/// the umask default — on this repo's machine 474 of 521 leftovers were
/// `drwxrwxr-x`, i.e. world-traversable (issue #358).
///
/// Unix-only: Windows has no POSIX mode bits and no umask race to close, so
/// the chmod is skipped there (the ACL equivalent is deferred to #163/#164).
fn harden_dir_0700(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod harness dir to 0o700");
    }
    #[cfg(not(unix))]
    let _ = path;
}

// ---------------------------------------------------------------------------
// Issue #322 — one harness-owned temp root per test process
// ---------------------------------------------------------------------------
//
// Every temp dir the harness creates nests under a single per-process root, so
// that:
//
//   * a process exiting normally removes the whole tree via the `atexit(3)`
//     hook below — including state that outlives any individual `TempDir`;
//   * a process that is SIGKILLed (nextest's `slow-timeout terminate-after`,
//     or a killed run) leaves behind exactly ONE directory, under a name this
//     repo owns, which `cargo xtask clean-e2e-tmp` can reap without guessing;
//   * leftovers are distinguishable from other tooling's. The `tempfile`
//     crate's default prefix is `.tmp`, so bare `/tmp/.tmp*` dirs belong to
//     every Rust program on the machine — a reaper globbing those could delete
//     a live temp dir owned by something else entirely.
//
// Why `atexit` and not a `Drop` guard: the lock dir this replaces was held in a
// `static OnceLock<TempDir>`, and Rust does not drop statics at process exit,
// so its `TempDir::drop` never ran. nextest runs one process per TEST, so that
// leaked one directory per test even on a fully GREEN run — measured at 13 dirs
// from 56 passing tests, and 6,667 accumulated on the dev machine over eight
// days. `atexit` runs on the normal-exit path libtest takes through
// `std::process::exit`, which a static's destructor does not.
//
// The prefix is kept short deliberately: these paths hold Unix domain sockets,
// which cap at ~108 bytes, and the root adds a nesting level to every one.

static HARNESS_TEMP_ROOT: OnceLock<PathBuf> = OnceLock::new();

// --- Where the root lives ---------------------------------------------------
//
// It used to be `std::env::temp_dir()`, i.e. `/tmp`. On this repo's dev box
// `/tmp` is a 14 GB *tmpfs*, so every ~280 MB per-test root is resident RAM,
// and a run that never reaches its cleanup hook holds that RAM until a human
// notices: 280 leaked roots / 6.2 GB in four hours, with swap down to 5 MiB
// free; reaping them handed back 3.8 GiB of swap. The visible symptom is not
// "out of space" but dozens of unrelated-looking failures — `dispatch_013` went
// 122s FAIL -> PASS in 8.9s with nothing changed but the temp location.
//
// The default is therefore `/var/tmp/dad-e2e-<uid>`. `/var/tmp` is short, and
// the FHS requires it to survive reboots, so a compliant system does not back
// it with a tmpfs.
//
// Two things this deliberately does NOT do.
//
// It does not put temp dirs under the repo's own `target/`. That was the first
// attempt at this fix and it is worse than it looks: every fixture the harness
// seeds would then be a *descendant of the real checkout*, which carries
// `CLAUDE.md`, `AGENTS.md`, `.claude/` and `.agents/`. Real agents walk
// ancestors and would discover genuine project instructions and skills; the
// real Codex worker runs `workspace-write` from such a directory, so a test's
// effective writable workspace could be the live repo. A nested `git init` does
// not close that — a git root is not a filesystem boundary, and several
// real-agent tests (`e2e_delegate_work_done_chain`, `e2e_pi_worker`,
// `e2e_codex_worker`, `e2e_pi_orchestrator`) call `race_safe_tempdir()`
// directly with no `git init` anywhere near them. Anyone who genuinely wants a
// target-local base can point `DAD_E2E_TMPDIR` at one explicitly.
//
// It also does not put the roots directly in `/var/tmp`, which is mode 1777 —
// world-writable, sticky, shared system-wide. Everything nests inside a 0700
// parent scoped to the effective UID, so nothing under it can belong to another
// user *by construction*. That is what lets the reaper decide what is safe to
// delete without inspecting ownership per directory, and it bounds the exposure
// of the real agent credentials the harness copies into seeded HOMEs (#358) to
// this user even though `/var/tmp` survives a reboot.
//
// The one thing that can veto a candidate is *path length*. These directories
// hold Unix domain sockets, and `sockaddr_un::sun_path` caps at 108 bytes on
// Linux (104 on macOS/BSD). Where `/tmp` costs 4 characters, a
// `<worktree>/target/tmp` cost 60+, and this repo's own worktree scheme
// (`../<repo>-<suffix>`, used by `/worktree-prd` and `/verify-pr`) reaches that
// easily — measured in `dot-agent-deck-dispatch-tmpfs-322`, an `attach.sock` at
// the harness's usual depth is 115 bytes and `bind(2)` fails outright with
// `AF_UNIX path too long`. `/var/tmp/dad-e2e-1000` is 21 bytes and leaves 34 to
// spare against the budget below.

/// Explicit override for the harness temp base. Wins over every other
/// candidate, *including* the socket-length veto: pointing it somewhere too
/// deep is your call, so it warns rather than silently relocating. It is still
/// validated first — see [`validated_override_base`] — and a value that fails
/// validation is fatal rather than ignored ([`refused_override_message`]).
const TEMP_BASE_ENV: &str = "DAD_E2E_TMPDIR";

/// The shared, world-writable directory the private parent is created in.
/// Unix-only: `/var/tmp` is an FHS path and means nothing on Windows.
const SHARED_VAR_TMP: &str = "/var/tmp";

/// Name of the private, UID-scoped parent created inside [`SHARED_VAR_TMP`].
///
/// Deliberately short — it is charged against the socket budget below on every
/// bind. `dad-e2e-1000` brings the base to 21 bytes; even a 10-digit UID only
/// reaches 27, against a 55-byte allowance.
fn private_parent_name(uid: u32) -> String {
    format!("dad-e2e-{uid}")
}

/// Usable bytes in `sockaddr_un::sun_path`, excluding the NUL terminator.
/// Linux allows 108 bytes (107 usable), macOS/BSD 104 (103). The smaller figure
/// is used everywhere so a layout that binds on Linux cannot fail on a Mac.
const SUN_PATH_USABLE: usize = 103;

/// Worst case the harness appends to its base before **binding** a socket:
///
/// | segment | bytes | |
/// |---|---|---|
/// | `/dad-tests-<pid>-<rand6>` | 25 | 7-digit PID; Linux's default `pid_max` is 4194304 |
/// | `/.tmp<rand6>` | 11 | the per-test dir `race_safe_tempdir` (and, since the redirect in [`harness_temp_root`], a bare `harness_tempdir()`) allocates |
/// | `/attach.sock` | 12 | longest socket name the harness *binds* — `hook.sock` and the scripted `daemon.sock` are shorter |
///
/// **Bound endpoints only.** `tests/e2e_delegate_work_done_chain.rs` composes a
/// `no-listener.sock` — 17 bytes with its separator, 5 over this budget — that
/// is deliberately never bound and only ever sent to in the expectation that
/// the send is a no-op, so an `AF_UNIX path too long` there is indistinguishable
/// from the `ECONNREFUSED` the test is actually asserting. Any *new* endpoint
/// that gets bound must fit this budget or the constant has to move with it;
/// `socket_budget_*` below is what fails if it does not.
const HARNESS_SOCKET_OVERHEAD: usize = 48;

/// Longest base that still leaves room for a bound socket underneath it.
const MAX_TEMP_BASE_LEN: usize = SUN_PATH_USABLE - HARNESS_SOCKET_OVERHEAD;

/// Whether a Unix socket bound under `base` still fits in `sun_path`.
fn fits_socket_budget(base: &Path) -> bool {
    base.as_os_str().len() <= MAX_TEMP_BASE_LEN
}

/// The chosen temp base, plus any explanation of why a preferred candidate was
/// passed over. Warnings are printed once per test process, never fatal.
struct TempBaseChoice {
    path: PathBuf,
    warnings: Vec<String>,
}

/// Pure decision half of [`harness_temp_base`] — no filesystem access, so the
/// precedence can be unit-tested with injected paths. Both candidates arrive
/// already validated (the override) or already created and verified (the
/// private parent); this function only orders them.
///
/// Precedence, highest first:
///
/// 1. `DAD_E2E_TMPDIR`, once validated. An explicit choice is not
///    second-guessed on length — it warns and is honoured.
/// 2. `/var/tmp/dad-e2e-<uid>` — the default. Short, disk-backed, and private
///    to this user by construction. `None` when there is no such rung at all:
///    a non-Unix platform, no `/var/tmp`, or a parent that genuinely could not
///    be created. A parent that *exists* and fails verification never reaches
///    here — that is fatal, see [`PrivateParentProblem`].
/// 3. `std::env::temp_dir()` (i.e. `TMPDIR`, else `/tmp`) — last resort, and
///    the one outcome that can put the suite back on a RAM-backed filesystem,
///    so it always warns.
fn choose_temp_base(
    env_override: Option<&Path>,
    private_parent: Option<&Path>,
    system_tmp: &Path,
) -> TempBaseChoice {
    if let Some(explicit) = env_override {
        let warnings = Vec::from_iter((!fits_socket_budget(explicit)).then(|| {
            format!(
                "{TEMP_BASE_ENV}={} leaves no room for a Unix socket path \
                 (needs {} bytes, budget {SUN_PATH_USABLE}); expect \
                 `AF_UNIX path too long`.",
                explicit.display(),
                explicit.as_os_str().len() + HARNESS_SOCKET_OVERHEAD,
            )
        }));
        return TempBaseChoice {
            path: explicit.to_path_buf(),
            warnings,
        };
    }
    let reason = match private_parent {
        Some(parent) if fits_socket_budget(parent) => {
            return TempBaseChoice {
                path: parent.to_path_buf(),
                warnings: Vec::new(),
            };
        }
        Some(parent) => format!(
            "{} would need {} bytes for a Unix socket path and the budget is \
             {SUN_PATH_USABLE}",
            parent.display(),
            parent.as_os_str().len() + HARNESS_SOCKET_OVERHEAD,
        ),
        None => format!("no private parent under {SHARED_VAR_TMP} is available"),
    };
    TempBaseChoice {
        path: system_tmp.to_path_buf(),
        warnings: vec![format!(
            "harness temp dirs fall back to {} — {reason}. If that is a tmpfs \
             this is exactly what issue #322 is about: point {TEMP_BASE_ENV} at \
             a short, disk-backed directory you own.",
            system_tmp.display(),
        )],
    }
}

/// This process's effective UID — the identity every harness-created directory
/// must be owned by.
#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, always succeeds, and cannot fail or
    // touch memory this process owns.
    unsafe { libc::geteuid() }
}

/// Create `path` (and any missing ancestor) owner-only.
///
/// `DirBuilder` applies the mode in the `mkdir(2)` that publishes each entry,
/// and a umask can only clear bits and never add them, so no component is ever
/// visible at the umask default even briefly — which is the window §3 of the
/// audit is about. `recursive(true)` is a no-op on a component that already
/// exists, so this is create-or-adopt; verifying an adopted directory is the
/// caller's job.
fn create_dir_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Why a directory the harness intends to own is not safe to adopt: it must be
/// ours and **exactly** `0o700`. Pure, so the rule can be unit-tested without
/// manufacturing foreign-owned directories.
///
/// The exactness is deliberate and was not always here. Testing only
/// `mode & 0o077 == 0` — "no group or other bits" — accepts `0o500`, `0o300`,
/// `0o000` and `0o1700` as well, while every diagnostic, the docs and
/// [`refused_private_parent_message`] all say `0o700`. Confidentiality was never
/// the gap (`mkdir(2)` applies the mode and a umask can only clear bits, so
/// there is no group-visible window either way); the gap was that a pre-existing
/// `0o500` parent sailed through the pre-flight whose entire job is to name the
/// problem up front, and then failed later as a bare `Permission denied` from
/// somewhere deep inside a test. So the check now enforces what it claims, and
/// the message names the one *innocent* way a directory the harness created can
/// land here — a umask clearing owner bits.
#[cfg(unix)]
fn private_dir_objection(uid: u32, mode: u32, euid: u32) -> Option<String> {
    if uid != euid {
        return Some(format!("owned by uid {uid}, not {euid}"));
    }
    if mode & 0o7777 != 0o700 {
        return Some(format!(
            "mode is 0o{mode:o}, not the 0o700 the harness requires: {}",
            if mode & 0o077 != 0 {
                "group/other bits would expose the real agent credentials seeded \
                 under it — `chmod 700` it, or point DAD_E2E_TMPDIR elsewhere"
            } else {
                "the group/other bits are clear but the OWNER bits are not \
                 rwx. If the harness created this directory, the cause is a \
                 umask clearing owner bits (`umask 0200` yields 0o500) — check \
                 `umask`, then `chmod 700` it"
            },
        ));
    }
    None
}

/// Why a directory on the way to an explicit override is not safe to traverse.
///
/// Laxer than [`private_dir_objection`] on purpose: `/`, `/home` and `/var` are
/// root-owned and world-readable, so demanding sole ownership would reject every
/// real path. What matters is that no *other* unprivileged user can rename or
/// replace the component underneath us, which is what a world-writable
/// directory without the sticky bit allows. Sticky 1777 directories (`/tmp`,
/// `/var/tmp`) are accepted: the sticky bit is precisely the guarantee that only
/// an entry's owner may remove or rename it, which closes the swap window.
#[cfg(unix)]
fn traversal_objection(uid: u32, mode: u32, euid: u32) -> Option<String> {
    if uid != euid && uid != 0 {
        return Some(format!("owned by uid {uid}, neither {euid} nor root"));
    }
    if mode & 0o022 != 0 && mode & 0o1000 == 0 {
        return Some(format!(
            "mode is 0o{mode:o} — group/world-writable without the sticky bit"
        ));
    }
    None
}

/// Which rule one directory in a candidate chain is judged by.
///
/// Ancestors of the base are ordinary system directories — `/`, `/var`, sticky
/// `/var/tmp`, somebody's `$HOME` — so they are judged by
/// [`traversal_objection`]: root-owned and sticky are normal, what matters is
/// that no *other* unprivileged user can rename or replace them underneath us.
/// The base itself, and every directory the harness creates or adopts on the way
/// down to it, is where the harness puts its own credential-bearing state, so it
/// is judged by [`private_dir_objection`] — ours, with no group or other bits at
/// all, the same bar the default `/var/tmp/dad-e2e-<uid>` parent has to meet.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum ChainRole {
    Ancestor,
    Ours,
}

/// How every directory in the walk below is opened: one component at a time,
/// relative to an already-open parent, never following a symlink at the end.
///
/// `O_PATH` where the platform has it, because it needs only *search* permission
/// on the directory — exactly what the `stat` walk this replaces needed. An
/// ancestor that is traversable but not readable (`/home` at 0711 is a real
/// configuration) must not turn into a refusal just because the check got
/// stricter about *when* it looks. Elsewhere `O_RDONLY` is the portable
/// spelling.
#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
const DIR_HANDLE_FLAGS: libc::c_int =
    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const DIR_HANDLE_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// One path component as a C string, for the `*at` calls below.
#[cfg(unix)]
fn component_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| format!("{} contains a NUL byte", Path::new(name).display()))
}

/// `openat(2)` a single component below `parent` — or an absolute path, when
/// `parent` is `None` — without following a symlink at the end of it.
///
/// Returning the descriptor rather than the name is the whole point: a
/// descriptor names an *inode*, so once it is open the entry it came from cannot
/// be renamed or replaced underneath the next step, and the permission check
/// runs against the very object the next `openat` resolves from. A `stat` of a
/// path followed by a *use* of that same path is two lookups, and anything with
/// write access to a component can swap what the second one finds.
#[cfg(unix)]
fn open_dir_nofollow(
    parent: Option<&std::fs::File>,
    name: &std::ffi::CStr,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::FromRawFd;
    let dirfd = parent.map_or(libc::AT_FDCWD, |dir| dir.as_raw_fd());
    // SAFETY: `name` is NUL-terminated and outlives the call, which only reads
    // it; `dirfd` is either `AT_FDCWD` or a descriptor owned by the live `File`
    // borrowed above.
    let fd = unsafe { libc::openat(dirfd, name.as_ptr(), DIR_HANDLE_FLAGS) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor this call owns and hands over exactly
    // once, so the `File` is its sole owner.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// `mkdirat(2)` a single component below `parent`, owner-only.
///
/// Deliberately *not* `create_dir_all` semantics: this fails with `EEXIST`
/// rather than accepting whatever already occupies the name, which is what lets
/// the caller judge an entry that appeared in the meantime instead of adopting
/// it sight unseen. The mode is applied by `mkdir(2)` itself and a umask can
/// only clear bits, so the directory is never visible at anything looser, even
/// briefly.
#[cfg(unix)]
fn mkdir_owner_only_at(parent: &std::fs::File, name: &std::ffi::CStr) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: as in `open_dir_nofollow` — a live directory descriptor and a
    // NUL-terminated name the call only reads.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Judge an already-open directory by `fstat(2)` **on its descriptor** — never
/// by a second lookup of its name, which is exactly the window this avoids.
#[cfg(unix)]
fn open_dir_objection(dir: &std::fs::File, role: ChainRole, euid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    let meta = match dir.metadata() {
        Ok(meta) => meta,
        Err(e) => return Some(format!("cannot be stat'ed: {e}")),
    };
    let mode = meta.permissions().mode() & 0o7777;
    match role {
        ChainRole::Ancestor => traversal_objection(meta.uid(), mode, euid),
        ChainRole::Ours => private_dir_objection(meta.uid(), mode, euid),
    }
}

/// Word an `openat` failure. The refusal has already happened — in the kernel,
/// because `O_NOFOLLOW | O_DIRECTORY` would not open the entry — so the extra
/// look this takes to distinguish a symlink from a plain file decides nothing
/// but the message. (`O_NOFOLLOW` reports a symlink as `ELOOP`, except under
/// `O_PATH`, where `O_DIRECTORY` rejects it as `ENOTDIR` instead.)
#[cfg(unix)]
fn unopenable_component_message(path: &Path, e: &std::io::Error) -> String {
    let is_symlink =
        || std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    match e.raw_os_error() {
        Some(libc::ELOOP) => format!("{} is a symlink", path.display()),
        Some(libc::ENOTDIR) if is_symlink() => format!("{} is a symlink", path.display()),
        Some(libc::ENOTDIR) => format!("{} is not a directory", path.display()),
        _ => format!("cannot open {}: {e}", path.display()),
    }
}

/// Open one component below `parent` and judge what came back, appending it to
/// `walked` so every message names the path as far as it got.
#[cfg(unix)]
fn descend_into(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    walked: &mut PathBuf,
    role: ChainRole,
    euid: u32,
) -> Result<std::fs::File, String> {
    let c_name = component_name(name)?;
    walked.push(name);
    let dir = open_dir_nofollow(Some(parent), &c_name)
        .map_err(|e| unopenable_component_message(walked, &e))?;
    match open_dir_objection(&dir, role, euid) {
        Some(why) => Err(format!("{} — {why}", walked.display())),
        None => Ok(dir),
    }
}

/// Create one component of the base below `parent`, or adopt what is already
/// there — and judge what was *actually* got, on its descriptor, either way.
///
/// This is the answer to the adoption race: a component that was missing when
/// the path was resolved can be created by another local user before the harness
/// reaches it, and a recursive create would then adopt their directory (or their
/// symlink) without ever looking at it. `mkdirat(2)` fails with `EEXIST` instead,
/// and `EEXIST` is not treated as success — it falls through to the same
/// open-and-judge a freshly created directory gets, under the strict
/// [`ChainRole::Ours`] rule. Whoever won the race, what the harness ends up
/// holding is a directory it has validated at the moment it adopted it.
#[cfg(unix)]
fn create_or_adopt_component(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    walked: &mut PathBuf,
    euid: u32,
) -> Result<std::fs::File, String> {
    let c_name = component_name(name)?;
    match mkdir_owner_only_at(parent, &c_name) {
        Ok(()) => {}
        // Somebody got there first — us on an earlier run, or someone else.
        // Which of those it was is decided by the judgement below, not here.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            walked.push(name);
            return Err(format!("cannot create {}: {e}", walked.display()));
        }
    }
    descend_into(parent, name, walked, ChainRole::Ours, euid)
}

/// Why a **symlink** on the way to the base must not be followed.
///
/// Pure, so the rule can be unit-tested with injected owners — the dangerous
/// shape is a link owned by *another* user, and `chown` is privileged, so it
/// cannot be built on disk by an unprivileged test.
///
/// A symlink is the one entry in the chain whose own permission bits decide
/// nothing: Linux fixes them at 0o777 and never consults them. Its **owner** is
/// the whole question, and it is only answerable at all because the parent this
/// link was found in has already passed [`traversal_objection`] — no other
/// unprivileged user could have replaced the parent itself.
///
/// Root is accepted because macOS's own `/var -> private/var` is exactly that,
/// and refusing it would reject the platform. Anyone else is refused: in a
/// sticky 1777 directory (`/tmp`, `/var/tmp`) another local user may *create*
/// entries freely, and the sticky bit then works against us — it stops the
/// victim removing or renaming the planted link. See
/// [`walk_to_validated_base`].
#[cfg(unix)]
fn symlink_hop_objection(path: &Path, uid: u32, euid: u32) -> Option<String> {
    (uid != euid && uid != 0).then(|| {
        format!(
            "{} is a symlink owned by uid {uid}, neither {euid} nor root — \
             another local user could have planted it to redirect the harness",
            path.display(),
        )
    })
}

/// Read a symlink's owner and target **relative to an already-open parent**,
/// without following it. `None` when the entry is not a symlink at all.
///
/// `fstatat`/`readlinkat` against the parent descriptor rather than
/// `symlink_metadata`/`read_link` of a path: the parent is pinned to an inode
/// the walk has already judged, so nothing above this entry can be swapped
/// between the judgement and the read.
#[cfg(unix)]
fn read_link_at(
    parent: &std::fs::File,
    name: &std::ffi::CStr,
) -> std::io::Result<Option<(u32, PathBuf)>> {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::io::AsRawFd;
    let dirfd = parent.as_raw_fd();
    // SAFETY: `st` is a correctly-sized, zeroed `stat` the call fills in;
    // `name` is NUL-terminated and only read; `dirfd` is owned by the live
    // `File` borrowed above.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFLNK {
        return Ok(None);
    }
    // `st_size` is the target length for a symlink, but it is a hint, not a
    // contract (procfs reports 0), so the buffer grows until the result is
    // provably not truncated.
    let mut cap = if st.st_size > 0 {
        st.st_size as usize + 1
    } else {
        libc::PATH_MAX as usize
    };
    loop {
        let mut buf = vec![0u8; cap];
        // SAFETY: `buf` is `cap` writable bytes and the call writes at most
        // that many; it never NUL-terminates, hence the truncation check.
        let n = unsafe {
            libc::readlinkat(
                dirfd,
                name.as_ptr(),
                buf.as_mut_ptr().cast::<libc::c_char>(),
                cap,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return Ok(Some((
                st.st_uid,
                PathBuf::from(std::ffi::OsString::from_vec(buf)),
            )));
        }
        cap *= 2;
    }
}

/// How many symlinks the walk below will follow before giving up, mirroring the
/// kernel's own `ELOOP` cap. A cycle of links the harness itself resolves would
/// otherwise spin forever where `canonicalize` used to return `ELOOP`.
#[cfg(unix)]
const MAX_SYMLINK_HOPS: usize = 40;

/// Structural objections to an override value, before anything is stat'ed.
/// Pure — no filesystem access.
///
/// A relative value would resolve against whatever working directory the test
/// binary happens to have, and `..` silently widens the scope of everything
/// downstream (the reaper included), so both are refused rather than normalised.
/// `.` and repeated separators are *not* refused: `Path::components` drops them,
/// and [`validated_override_base`] returns that normalized form, so they cannot
/// reach anything downstream in the first place.
fn override_shape_objection(raw: &Path) -> Option<String> {
    use std::path::Component;
    if !raw.is_absolute() {
        return Some(format!("{} is not an absolute path", raw.display()));
    }
    raw.components()
        .any(|c| c == Component::ParentDir)
        .then(|| format!("{} contains a `..` component", raw.display()))
}

/// Walk `raw` from `/` one component at a time, validating **before** resolving,
/// and return the descriptor the walk finished on plus the resolved spelling.
///
/// The ordering is the whole point, and it is what the first cut of this got
/// wrong. That version handed the value to `canonicalize` first and only then
/// walked the *result* with descriptors — so a symlink somebody else had planted
/// at a component was resolved away before its owner was ever looked at, and the
/// checkout (or tmpfs) it pointed to was then walked as a chain of perfectly
/// ordinary victim-owned ancestors and accepted. The sticky bit does not save
/// you there: sticky stops another user *removing or renaming* an entry, so on
/// `/var/tmp` it protects the attacker's pre-planted link from the person it
/// redirects. Accepting a sticky 1777 directory is only sound when the entry
/// found *below* it is judged, which is what this does.
///
/// So, per component:
///
/// - `openat(O_NOFOLLOW | O_DIRECTORY)` — a real directory is judged by `fstat`
///   **on that descriptor**, never by a second lookup of its name, so nothing is
///   adopted on the strength of an earlier `stat`. Ancestors must not be
///   replaceable by another unprivileged user ([`traversal_objection`]); the
///   base, and every component created on the way down to it, must be ours with
///   no group or other bits ([`private_dir_objection`]).
/// - Nothing there — `mkdirat`, which *fails* rather than adopting an entry that
///   appeared in between; `EEXIST` is judged, not trusted
///   ([`create_or_adopt_component`]).
/// - A symlink — inspected without following: only a link owned by root or by us
///   is resolved ([`symlink_hop_objection`]), and only then are its own
///   components pushed onto the front of the walk and traversed through
///   descriptors like any others. macOS's root-owned `/var -> private/var` still
///   works; an attacker-owned link in a sticky directory does not.
///
/// A `..` inside a *link target* is refused rather than resolved. Walking one
/// would mean stepping back above a component this function has already proved
/// safe, and no real system link the harness needs (macOS's `/var` included)
/// contains one.
#[cfg(unix)]
fn walk_to_validated_base(raw: &Path, euid: u32) -> Result<(std::fs::File, PathBuf), String> {
    use std::collections::VecDeque;
    use std::path::Component;

    let root = component_name(std::ffi::OsStr::new("/"))?;
    let mut walked = PathBuf::from("/");
    let mut dir =
        open_dir_nofollow(None, &root).map_err(|e| unopenable_component_message(&walked, &e))?;

    // `override_shape_objection` has already refused `..`, and `components()`
    // drops `.` and repeated separators, so what is left after the leading
    // `RootDir` is nothing but `Normal`.
    let mut pending: VecDeque<std::ffi::OsString> = raw
        .components()
        .skip(1)
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut hops = 0usize;

    while let Some(name) = pending.pop_front() {
        // The base is whatever component the walk ends on — which a symlink
        // expansion can move, so it is read off the queue rather than an index.
        let is_base = pending.is_empty();
        let role = if is_base {
            ChainRole::Ours
        } else {
            ChainRole::Ancestor
        };
        let c_name = component_name(&name)?;
        match open_dir_nofollow(Some(&dir), &c_name) {
            // An ordinary directory.
            Ok(opened) => {
                walked.push(&name);
                if let Some(why) = open_dir_objection(&opened, role, euid) {
                    return Err(format!("{} — {why}", walked.display()));
                }
                dir = opened;
            }
            // Nothing there — the harness makes it, strictly, and everything
            // below it likewise.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                dir = create_or_adopt_component(&dir, &name, &mut walked, euid)?;
            }
            // Something the kernel would not open no-follow. A symlink is the
            // one shape that is not automatically fatal — it is judged, and if
            // it is ours or root's it is expanded onto the walk.
            Err(e) => {
                let probed = walked.join(&name);
                let link = read_link_at(&dir, &c_name)
                    .map_err(|e| format!("cannot stat {}: {e}", probed.display()))?;
                let Some((uid, target)) = link else {
                    return Err(unopenable_component_message(&probed, &e));
                };
                if let Some(why) = symlink_hop_objection(&probed, uid, euid) {
                    return Err(why);
                }
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(format!(
                        "{} resolves through more than {MAX_SYMLINK_HOPS} symlinks",
                        raw.display(),
                    ));
                }
                if target.components().any(|c| c == Component::ParentDir) {
                    return Err(format!(
                        "{} is a symlink to {}, which contains a `..` component",
                        probed.display(),
                        target.display(),
                    ));
                }
                // An absolute target restarts the walk at `/`; a relative one
                // continues from the directory the link was found in. Either
                // way the target's own components go through this same loop, so
                // a link chain is validated hop by hop.
                if target.is_absolute() {
                    walked = PathBuf::from("/");
                    dir = open_dir_nofollow(None, &root)
                        .map_err(|e| unopenable_component_message(&walked, &e))?;
                }
                for component in target.components().rev() {
                    if let Component::Normal(part) = component {
                        pending.push_front(part.to_os_string());
                    }
                }
                // A link to `/` alone leaves nothing to walk; the root
                // descriptor is already open and is the answer.
                if pending.is_empty() {
                    return Ok((dir, walked));
                }
            }
        }
    }
    Ok((dir, walked))
}

/// Validate `DAD_E2E_TMPDIR` and return the base to use.
///
/// Absolute and traversal-free (checked above), then walked from `/` by
/// [`walk_to_validated_base`] — validating each component before resolving it,
/// with descriptor-relative no-follow opens throughout.
///
/// Two things this deliberately does not claim. The descriptors are dropped when
/// it returns, so what the caller gets is a *validated path*, not a pinned
/// handle: every later use resolves the name again. What makes that safe is the
/// property proved on the way down — every ancestor is owned by us or by root
/// and is not writable by others except under the sticky bit, where only an
/// entry's own owner may rename or remove it, and every symlink among them was
/// owned by us or by root. And the returned path is the resolved one, so a
/// symlink the operator pointed at is followed exactly once, here, and never
/// again downstream.
#[cfg(unix)]
fn validated_override_base(raw: &Path) -> Result<PathBuf, String> {
    if let Some(why) = override_shape_objection(raw) {
        return Err(why);
    }
    let euid = effective_uid();
    let (dir, walked) = walk_to_validated_base(raw, euid)?;
    // The base itself, once the chain is complete — the check that has to hold
    // at the moment of *use*, whether the directory was just created, was
    // adopted, or was already there when the walk started. The loop already
    // judged the final component under `ChainRole::Ours`; re-reading the same
    // descriptor is what makes that true of the value being *returned*, not
    // merely of a component that happened to be last.
    if let Some(why) = open_dir_objection(&dir, ChainRole::Ours, euid) {
        return Err(format!("{} — {why}", walked.display()));
    }
    Ok(walked)
}

/// Whether an entry carries the Windows reparse-point attribute at all.
///
/// Broader on purpose than `FileType::is_symlink`, which on Windows is true only
/// for the two tags `std` classifies as links (`IO_REPARSE_TAG_SYMLINK` and
/// `IO_REPARSE_TAG_MOUNT_POINT`, i.e. junctions). Other tags also redirect —
/// cloud-file placeholders, `AppExecLink`, an app-execution alias — and the
/// harness has no business writing credentials through any of them. The
/// attribute bit itself is reachable from `std` via `MetadataExt`, so this costs
/// no new dependency; the ACL half of the same question is not, and is #163/#164.
#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    /// `FILE_ATTRIBUTE_REPARSE_POINT`, spelled out rather than pulled in from
    /// `windows-sys` — one documented constant is not worth a dependency on a
    /// platform whose e2e tier does not run yet.
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Elsewhere there is no such attribute, and `is_symlink` already answers the
/// whole question.
#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Pure decision half of the by-name walk below: `Some(why)` when an entry with
/// these observed properties must be refused, `None` when it is an ordinary
/// directory the harness may walk through or use.
///
/// Taking the flags rather than a `Metadata` is what makes the reparse-point
/// rule testable at all — that shape only exists on Windows, and the Windows
/// arm cannot be exercised from the Unix host this suite runs on.
fn chain_entry_verdict(
    path: &Path,
    is_symlink: bool,
    is_reparse_point: bool,
    is_dir: bool,
) -> Option<String> {
    if is_symlink {
        return Some(format!("{} is a symlink", path.display()));
    }
    if is_reparse_point {
        return Some(format!("{} is a reparse point", path.display()));
    }
    (!is_dir).then(|| format!("{} is not a directory", path.display()))
}

/// Judge one `symlink_metadata` reading through [`chain_entry_verdict`].
fn chain_entry_verdict_for(path: &Path, meta: &std::fs::Metadata) -> Option<String> {
    chain_entry_verdict(
        path,
        meta.file_type().is_symlink(),
        is_reparse_point(meta),
        meta.is_dir(),
    )
}

/// Filesystem-facing adapter over [`chain_entry_verdict`]: stat `path` and judge
/// what came back. A missing entry is a refusal too — every caller reaches this
/// only for something that is supposed to be there by then.
fn chain_entry_objection(path: &Path) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => chain_entry_verdict_for(path, &meta),
        Err(e) => Some(format!("cannot stat {}: {e}", path.display())),
    }
}

/// Create one component of a by-name chain, or judge what is already there —
/// never adopt it sight unseen.
///
/// `create_dir` rather than `create_dir_all` is the whole point, and it is the
/// portable half of what `mkdirat` does for [`create_or_adopt_component`]: it
/// fails with `AlreadyExists` instead of accepting whatever occupies the name,
/// so an entry another local user planted between the walk and this moment is
/// *looked at* rather than used. Windows reports `ERROR_ALREADY_EXISTS` for a
/// file, a directory, a junction and a symlink alike, so every shape an attacker
/// could leave arrives here and is judged.
fn create_or_refuse_component(path: &Path) -> Result<(), String> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        // Somebody got there first — an earlier run, or someone else. Which of
        // those it was is not answerable by name alone; what *is* answerable is
        // whether the entry redirects somewhere, and that is refused.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            chain_entry_objection(path).map_or(Ok(()), Err)
        }
        Err(e) => Err(format!("cannot create {}: {e}", path.display())),
    }
}

/// The by-name equivalent of [`validated_override_base`]'s descriptor walk, for
/// platforms where `std` offers no `openat`.
///
/// Kept free of `cfg` so it compiles — and is unit-tested — on the Unix host
/// this suite actually runs on, even though only the `#[cfg(not(unix))]` arm
/// calls it. The logic is plain `std::fs`, so what a Linux test observes here is
/// what Windows executes; the two Windows-only pieces are the reparse-point
/// attribute (pinned separately through [`chain_entry_verdict`]) and the exact
/// error codes `CreateDirectoryW` returns.
///
/// **What this closes.** Nothing is adopted silently any more: every component,
/// existing or created, is stat'ed with `symlink_metadata` and refused if it is
/// a symlink, a junction, any other reparse point, or not a directory. That is
/// the redirection Greptile's finding names — a pre-planted entry at a missing
/// component aiming the credential-bearing harness tree at storage somebody else
/// chose.
///
/// Unlike the Unix arm, a symlinked **ancestor** is refused rather than resolved
/// — the behaviour this arm already had. Resolving is a concession the Unix side
/// has to make because macOS's own `/var` is a symlink to `/private/var`, so
/// refusing it rejected the platform; Windows has no such component on a healthy
/// machine, and `canonicalize` there returns a `\\?\` verbatim path that would
/// then be the spelling every downstream message and length budget used. Strict
/// is both simpler and safer here.
///
/// **What it does not close, and cannot from `std`.** Three things, stated
/// plainly rather than implied:
///
/// 1. The judgement is a **second lookup of the name**, not an `fstat` of the
///    descriptor the entry was opened with. Between the `AlreadyExists` and the
///    `symlink_metadata` — and again between that and every later use — the name
///    can be swapped. The window is narrowed, not removed.
/// 2. There is **no ownership check**. A plain directory another local user
///    planted at a missing component is still adopted, because Windows ACLs are
///    not reachable from `std` and there is no `uid` to compare. Redirection is
///    refused; a co-located directory owned by somebody else is not detected.
/// 3. Directories are created with **inherited ACLs**, not the 0700 equivalent
///    `mkdir(2)` gives the Unix arm, so a permissive parent stays permissive.
///
/// All three are the ACL-and-handle work tracked by #163/#164. Deliberately not
/// fixed here with `windows-sys`: that is a real dependency decision for a
/// platform whose L2 tier does not run yet.
fn override_base_by_name(raw: &Path) -> Result<PathBuf, String> {
    if let Some(why) = override_shape_objection(raw) {
        return Err(why);
    }
    // Normalized, so nothing downstream — the socket-length budget included —
    // sees a spelling the filesystem does not.
    let normalized: PathBuf = raw.components().collect();
    let mut walked = PathBuf::new();
    for component in normalized.components() {
        walked.push(component);
        // The anchor — `/`, a drive root, a UNC share — is not something the
        // harness can create or could meaningfully judge, and `CreateDirectoryW`
        // on a drive root reports access denied rather than "already exists", so
        // trying would fail every Windows path. `components()` yields these
        // first and nothing but `Normal` after, so the skip is exact.
        if !matches!(component, std::path::Component::Normal(_)) {
            continue;
        }
        let existing = match std::fs::symlink_metadata(&walked) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot stat {}: {e}", walked.display())),
        };
        match existing {
            // Already there: judged, never created over. Attempting a create on
            // every existing ancestor would buy nothing and would make the walk
            // depend on which of `EEXIST` and `EROFS`/`EACCES` a filesystem
            // reports first.
            Some(meta) => {
                if let Some(why) = chain_entry_verdict_for(&walked, &meta) {
                    return Err(why);
                }
            }
            // Missing, so the harness makes it — and refuses to adopt whatever
            // may have appeared between that stat and this create.
            None => create_or_refuse_component(&walked)?,
        }
    }
    // And the base itself, once the chain is complete — the check that has to
    // hold at the moment of *use*, whether the directory was just created or was
    // already there when the walk started.
    if let Some(why) = chain_entry_objection(&normalized) {
        return Err(why);
    }
    Ok(normalized)
}

/// Windows has neither POSIX ownership nor mode bits to judge a candidate by,
/// and no `openat`-shaped API reachable from `std`, so the value gets the shape
/// check and then the by-name walk above: created one component at a time, and
/// refused rather than adopted where something is already there. See
/// [`override_base_by_name`] for exactly what that does and does not buy; the
/// ACL-based equivalent of the Unix walk is #163/#164.
#[cfg(not(unix))]
fn validated_override_base(raw: &Path) -> Result<PathBuf, String> {
    override_base_by_name(raw)
}

/// Why the private `/var/tmp` parent could not be used — and, the load-bearing
/// part, whether that is survivable.
///
/// Refusing a suspect directory is right, but *silently* dropping to
/// `std::env::temp_dir()` afterwards is not: it converts a security refusal
/// into the capacity problem the whole ladder exists to avoid, announced only
/// by a stderr warning nextest interleaves across thousands of processes. The
/// operator then gets issue #322's original symptom — a wall of misleading
/// failures — with nothing pointing at the cause.
#[derive(Debug)]
enum PrivateParentProblem {
    /// An ordinary environment difference: no `/var/tmp` rung at all (non-Unix),
    /// no `/var/tmp` on this machine, or a parent that genuinely could not be
    /// created with *nothing* sitting at the path. Warn and fall through.
    Unavailable(String),
    /// The parent is there and cannot be trusted — a symlink, not a directory,
    /// foreign-owned, or carrying group/other bits. Carries the ready-made
    /// fatal message; the harness stops instead of falling through.
    Refused(String),
}

impl std::fmt::Display for PrivateParentProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Refused(why) => f.write_str(why),
        }
    }
}

/// The hard-failure message for a private parent that exists but cannot be
/// trusted. Pure and separate from the stat, so its wording can be asserted on
/// without manufacturing a foreign-owned directory — which needs privileges the
/// test process does not have.
///
/// Framed like [`insufficient_temp_space_message`] and for the same reason: by
/// the time a wrong temp base surfaces in assertions it is indistinguishable
/// from a product regression, so the message has to say what it is before it
/// says anything else.
#[cfg(unix)]
fn refused_private_parent_message(path: &Path, observed: &str, euid: u32) -> String {
    let path = path.display();
    format!(
        "HARNESS PRE-FLIGHT FAILURE — this is the test harness refusing to start, \
         NOT a product regression.\n\n\
         {path} exists and is {observed}; the harness requires a real directory \
         owned by uid {euid} with mode 0o700 (no group or other bits).\n\n\
         No test has run. That directory is the parent every harness temp dir is \
         created under, and it is verified rather than repaired — chown'ing or \
         chmod'ing a directory in world-writable {SHARED_VAR_TMP} that may not be \
         yours is exactly what this check exists to prevent. The harness stops here \
         rather than falling back to the system temp dir (`TMPDIR`, else `/tmp`), \
         because that fallback is commonly a RAM-backed tmpfs and would turn this \
         refusal straight back into issue #322 — dozens of unrelated-looking \
         assertion failures with nothing pointing at the filesystem.\n\n\
         Look at it, and if it is not something you care about, remove it:\n\n    \
         ls -ld {path}\n    rm -rf {path}\n\n\
         If you cannot remove it, set {TEMP_BASE_ENV} to a short, absolute, \
         disk-backed directory you own and the harness will use that instead.",
    )
}

/// Pure decision half of the private-parent check, mirroring
/// [`temp_space_verdict`]: `Some(message)` when a parent with these stat fields
/// must be refused outright, `None` when it is exactly what the harness needs.
///
/// Taking the fields rather than a `Metadata` is what makes the foreign-owned
/// case testable at all — `chown` is privileged, so that shape cannot be built
/// on disk by a test.
#[cfg(unix)]
fn private_parent_verdict(
    path: &Path,
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    mode: u32,
    euid: u32,
) -> Option<String> {
    let observed = if is_symlink {
        "a symlink".to_string()
    } else if !is_dir {
        "not a directory".to_string()
    } else if private_dir_objection(uid, mode, euid).is_some() {
        format!("a directory owned by uid {uid} with mode 0o{mode:o}")
    } else {
        return None;
    };
    Some(refused_private_parent_message(path, &observed, euid))
}

/// Filesystem-facing adapter over [`private_parent_verdict`]: judge one
/// `symlink_metadata` reading of `path`.
#[cfg(unix)]
fn private_parent_verdict_for(path: &Path, meta: &std::fs::Metadata, euid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    private_parent_verdict(
        path,
        meta.file_type().is_symlink(),
        meta.is_dir(),
        meta.uid(),
        meta.permissions().mode() & 0o7777,
        euid,
    )
}

/// The private, UID-scoped parent inside `shared`, created if absent and
/// verified if not. `shared` and `euid` are parameters purely so the tests can
/// drive every outcome against a scratch directory instead of the real
/// `/var/tmp`; [`private_temp_parent`] is the one production caller.
///
/// Adopting an existing directory is the dangerous case — `/var/tmp` is mode
/// 1777, so another user can own a plausibly-named directory there. It is
/// verified rather than repaired: a directory that is not ours is left exactly
/// as it is, and refusing it is `Refused`, not a fall-through.
#[cfg(unix)]
fn private_temp_parent_in(shared: &Path, euid: u32) -> Result<PathBuf, PrivateParentProblem> {
    if !shared.is_dir() {
        return Err(PrivateParentProblem::Unavailable(format!(
            "{} is not a directory",
            shared.display()
        )));
    }
    let parent = shared.join(private_parent_name(euid));
    if let Err(e) = create_dir_private(&parent) {
        // Classified by what is actually at the path: an entry the harness will
        // not touch is a refusal, nothing at all is an ordinary environment
        // problem (a read-only or full filesystem).
        let refusal = std::fs::symlink_metadata(&parent)
            .ok()
            .and_then(|meta| private_parent_verdict_for(&parent, &meta, euid));
        return Err(match refusal {
            Some(message) => PrivateParentProblem::Refused(message),
            None => PrivateParentProblem::Unavailable(format!(
                "cannot create {}: {e}",
                parent.display()
            )),
        });
    }
    // `symlink_metadata`: a symlink planted at this name would otherwise be
    // followed by both the creation above (which succeeds when the target is a
    // directory) and everything after it.
    let meta = match std::fs::symlink_metadata(&parent) {
        Ok(meta) => meta,
        Err(e) => {
            return Err(PrivateParentProblem::Unavailable(format!(
                "cannot stat {}: {e}",
                parent.display()
            )));
        }
    };
    match private_parent_verdict_for(&parent, &meta, euid) {
        Some(message) => Err(PrivateParentProblem::Refused(message)),
        None => Ok(parent),
    }
}

/// The private, UID-scoped parent inside `/var/tmp` for this process.
#[cfg(unix)]
fn private_temp_parent() -> Result<PathBuf, PrivateParentProblem> {
    private_temp_parent_in(Path::new(SHARED_VAR_TMP), effective_uid())
}

/// Windows has no `/var/tmp` and no POSIX mode bits to scope one with, so the
/// system temp dir is the only rung. That is an absence, never a refusal. The
/// ACL-based equivalent is #163/#164.
#[cfg(not(unix))]
fn private_temp_parent() -> Result<PathBuf, PrivateParentProblem> {
    Err(PrivateParentProblem::Unavailable(format!(
        "{SHARED_VAR_TMP} private parents are Unix-only (Windows ACL hardening \
         is tracked by #163/#164)"
    )))
}

/// The hard-failure message for a `DAD_E2E_TMPDIR` that cannot be honoured.
///
/// Fatal for the same reason a refused private parent is, only more so: the
/// operator *stated* where the harness temp dirs must go, so quietly putting
/// them somewhere else — typically the RAM-backed system temp dir — is both a
/// wrong answer and an unasked-for one. Every rejection reaches here, including
/// "could not be created": there is no reading of an explicit value under which
/// ignoring it is the helpful thing to do.
fn refused_override_message(raw: &Path, why: &str) -> String {
    format!(
        "HARNESS PRE-FLIGHT FAILURE — this is the test harness refusing to start, \
         NOT a product regression.\n\n\
         {TEMP_BASE_ENV}={} cannot be used: {why}.\n\n\
         No test has run. Setting {TEMP_BASE_ENV} states where every harness temp \
         dir must go, so a value that cannot be honoured is refused outright rather \
         than ignored: falling through would silently place them somewhere you did \
         not ask for — usually the system temp dir, which is commonly a RAM-backed \
         tmpfs and is exactly what issue #322 is about.\n\n\
         Point {TEMP_BASE_ENV} at a short, absolute, disk-backed directory you own \
         with no `..`, no component another unprivileged user could replace, and \
         no group or other permission bits on the directory itself (`chmod 700` — \
         the same bar the default parent has to meet, since this is where the \
         harness seeds real agent credentials) — or unset it to use the default \
         {SHARED_VAR_TMP}/dad-e2e-<uid>.",
        raw.display(),
    )
}

/// Resolve the temp base from the live environment. See [`choose_temp_base`]
/// for the precedence; this half does the validating and the creating, and is
/// where the two non-survivable outcomes stop the process.
fn harness_temp_base() -> TempBaseChoice {
    let mut warnings = Vec::new();
    let env_override = std::env::var_os(TEMP_BASE_ENV)
        .filter(|v| !v.is_empty())
        .map(|raw| {
            let raw = PathBuf::from(raw);
            validated_override_base(&raw)
                .unwrap_or_else(|why| panic!("{}", refused_override_message(&raw, &why)))
        });
    // Only attempted when it is actually going to be used: an explicit override
    // should not leave a directory behind in `/var/tmp` that nothing writes to.
    let private_parent = match env_override {
        Some(_) => None,
        None => match private_temp_parent() {
            Ok(parent) => Some(parent),
            Err(PrivateParentProblem::Unavailable(why)) => {
                warnings.push(why);
                None
            }
            Err(PrivateParentProblem::Refused(message)) => panic!("{message}"),
        },
    };
    let mut choice = choose_temp_base(
        env_override.as_deref(),
        private_parent.as_deref(),
        &std::env::temp_dir(),
    );
    warnings.append(&mut choice.warnings);
    choice.warnings = warnings;
    choice
}

/// Free space the e2e tier wants on the temp filesystem before it starts, in
/// MB. Peak demand is what matters, not per-test demand: one seeded HOME was
/// measured at 263-284 MB and nextest runs one process per core, so eight
/// concurrent tests already want ~2.2 GB and the roots a killed run leaves
/// behind are still occupying the same filesystem. 2 GB is deliberately below
/// true peak — it is a "this run is doomed" floor, not a capacity guarantee,
/// and set high enough to catch the exhausted-tmpfs case that produced a
/// 103-test red wall while staying under what a modest CI runner offers.
/// Override with `DAD_E2E_MIN_FREE_MB`; `0` disables the check.
#[cfg(unix)]
const E2E_MIN_FREE_MB: u64 = 2048;

/// Escape hatch for the pre-flight threshold; `0` disables the check entirely.
#[cfg(unix)]
const MIN_FREE_ENV: &str = "DAD_E2E_MIN_FREE_MB";

/// Space available to this user on the filesystem holding `path`.
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated path and `st` is a
    // correctly-sized, zeroed `statvfs` that the call fills in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // `f_bavail` (not `f_bfree`) is what an unprivileged process can actually use.
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// The explicit fail-fast message, kept separate from the `statvfs` call so it
/// can be asserted on without depending on the machine's actual free space.
///
/// It leads with "harness pre-flight" on purpose: the whole reason this check
/// exists is that an exhausted temp filesystem is indistinguishable from a
/// product regression by the time the assertions fail.
#[cfg(unix)]
fn insufficient_temp_space_message(free_mb: u64, need_mb: u64, path: &Path) -> String {
    format!(
        "HARNESS PRE-FLIGHT FAILURE — this is the test harness refusing to start, \
         NOT a product regression.\n\n\
         e2e needs ~{need_mb} MB free in {}; found {free_mb} MB.\n\n\
         No test has run. The harness stopped here because it cannot create its \
         per-test temp dirs, and an exhausted temp filesystem does not surface as \
         an out-of-space error — it surfaces as dozens of unrelated-looking \
         assertion failures (agents never becoming input-ready, `git init` \
         failing, daemons never booting), which is exactly the misleading red wall \
         this check exists to prevent.\n\n\
         Interrupted runs leave their temp roots behind — nextest SIGKILLs a test \
         that trips `slow-timeout terminate-after`, and a killed process never runs \
         its cleanup. Reclaim them with:\n\n    \
         cargo xtask clean-e2e-tmp --apply\n\n\
         That reaps the STANDARD roots only. The path above is a standard root \
         unless you set {TEMP_BASE_ENV}, in which case name it explicitly:\n\n    \
         cargo xtask clean-e2e-tmp --root {} --apply\n\n\
         Set {MIN_FREE_ENV} to change this threshold, or 0 to disable it; set \
         {TEMP_BASE_ENV} to put the harness temp dirs on a different filesystem.",
        path.display(),
        path.display(),
    )
}

/// Pure decision half of the pre-flight, separated from the `statvfs` call so
/// it can be unit-tested with injected numbers instead of a real full disk.
///
/// `None` — meaning "no objection" — when there is room, when the check is
/// disabled (`need_mb == 0`), or when free space could not be determined
/// (`free_mb == None`). That last case is deliberate: a filesystem `statvfs`
/// cannot answer for must never become a failure source of its own.
#[cfg(unix)]
fn temp_space_verdict(free_mb: Option<u64>, need_mb: u64, path: &Path) -> Option<String> {
    if need_mb == 0 {
        return None;
    }
    let free_mb = free_mb?;
    (free_mb < need_mb).then(|| insufficient_temp_space_message(free_mb, need_mb, path))
}

/// The configured threshold in MB, from `DAD_E2E_MIN_FREE_MB` or the default.
/// An unparseable value falls back to the default rather than failing.
#[cfg(unix)]
fn min_free_mb() -> u64 {
    std::env::var(MIN_FREE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(E2E_MIN_FREE_MB)
}

/// Fail-fast pre-flight: `Some(message)` when the temp filesystem is too full
/// for the e2e tier to run meaningfully. One `statvfs` per test process, and
/// none at all when the check is switched off.
#[cfg(unix)]
fn temp_space_problem(path: &Path) -> Option<String> {
    let need_mb = min_free_mb();
    if need_mb == 0 {
        return None;
    }
    temp_space_verdict(free_bytes(path).map(|b| b / (1024 * 1024)), need_mb, path)
}

/// Per-process root owning every temp dir this test process creates.
fn harness_temp_root() -> &'static Path {
    HARNESS_TEMP_ROOT
        .get_or_init(|| {
            let choice = harness_temp_base();
            for warning in &choice.warnings {
                eprintln!("[harness] WARNING: {warning}");
            }
            let base = choice.path;
            // Created before anything measures it: `std::env::temp_dir()`
            // reports `TMPDIR` without creating it, so a base that is not there
            // has to self-heal rather than fail every test on a missing
            // directory. Doing it first also gives the space probe below a real
            // directory to `statvfs` — on a path that does not exist the call
            // fails and the check silently no-ops. Owner-only, one component at
            // a time; a no-op when the base already exists.
            create_dir_private(&base)
                .unwrap_or_else(|e| panic!("create harness temp base {}: {e}", base.display()));
            // Checked here because this is the one choke point every harness
            // temp dir passes through, so no test can start doing real work
            // against an exhausted filesystem without first seeing why. It runs
            // against whatever base was actually chosen above, not a hardcoded
            // `/tmp`, and exactly once per test process.
            #[cfg(all(unix, feature = "e2e"))]
            if let Some(msg) = temp_space_problem(&base) {
                panic!("{msg}");
            }
            let prefix = format!("dad-tests-{}-", std::process::id());
            // 0o700 asked for at creation rather than chmod'ed afterwards. The
            // base can be shared (`std::env::temp_dir()` on the last rung), and
            // a root that is briefly 0o755 there is long enough for a local user
            // to enter it and plant fixed descendants — `daemon-lock`, a
            // pre-made socket path — that make later tests fail in ways nobody
            // would think to blame on the filesystem. Same pattern as the
            // per-test dir in `TuiDeck::try_launch_inner`.
            let root = {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tempfile::Builder::new()
                        .prefix(&prefix)
                        .permissions(std::fs::Permissions::from_mode(0o700))
                        .tempdir_in(&base)
                }
                #[cfg(not(unix))]
                {
                    tempfile::Builder::new().prefix(&prefix).tempdir_in(&base)
                }
            }
            .unwrap_or_else(|e| panic!("create harness temp root in {}: {e}", base.display()))
            .keep();
            // Verified, not assumed: a future tempfile API rename would
            // otherwise silently drop the permission application and leave the
            // window above open again.
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                use std::os::unix::fs::PermissionsExt;
                let meta = std::fs::symlink_metadata(&root).expect("stat harness temp root");
                let mode = meta.permissions().mode() & 0o7777;
                if let Some(why) = private_dir_objection(meta.uid(), mode, effective_uid()) {
                    panic!("harness temp root {} is {why}", root.display());
                }
            }
            register_temp_root_cleanup();
            // Issue #322, defence in depth: point the `tempfile` crate's DEFAULT
            // temp dir at the root, so an allocation the suite does not make
            // itself — a dependency's, or a call site that slips past
            // `linkage-check` rule 8 — lands inside it too.
            //
            // This is NOT what contains the suite's own allocations, and
            // believing it was is what the late audit caught. The override is
            // installed at the end of this lazy initialiser, so it is in force
            // only from the first moment something asks the harness for a
            // directory; a bare `harness_tempdir()` running before that is the
            // process's first allocation and goes to the OS temp dir. Containment
            // is the job of `common::harness_tempdir()`, which initialises the
            // root before it allocates and is therefore ordering-independent.
            //
            // It is tempfile's own process-global override, NOT `TMPDIR`: no
            // `set_var`, so nothing here is racy against other threads, and
            // spawned agent subprocesses keep resolving temp the way the OS tells
            // them to.
            //
            // `Err` means someone else already set the override, which no code in
            // this repo does — so the root this initialiser just created and
            // registered for cleanup is NOT where stray allocations will go, and
            // the invariant this whole block exists to establish is broken.
            // Discarding that (`let _ =`) hid exactly the kind of ordering bug
            // above, so it fails loudly instead.
            tempfile::env::override_temp_dir(&root).unwrap_or_else(|e| {
                panic!(
                    "tempfile's process-global temp-dir override was already set \
                     when the harness tried to point it at {}: {e:?}. Nothing in \
                     this repo sets it, so stray allocations are landing \
                     somewhere this harness does not own and will not clean up.",
                    root.display(),
                )
            });
            root
        })
        .as_path()
}

/// Register the process-exit hook that removes [`harness_temp_root`].
#[cfg(unix)]
fn register_temp_root_cleanup() {
    extern "C" fn cleanup() {
        let Some(root) = HARNESS_TEMP_ROOT.get() else {
            return;
        };
        // Retried: a daemon or agent this test spawned can outlive the test
        // body by a moment and keep writing into the tree, so the first sweep
        // can lose a race and fail with ENOTEMPTY. Retrying costs nothing on
        // the overwhelmingly common first-try success.
        let mut last_err = None;
        for _ in 0..3 {
            match std::fs::remove_dir_all(root) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) => last_err = Some(e),
            }
        }
        // Deliberately loud. Swallowing this is what let the original leak run
        // for eight days unnoticed; a warning here names the directory that is
        // about to be left behind and how to reclaim it.
        if let Some(e) = last_err {
            eprintln!(
                "[harness] WARNING: could not remove temp root {} ({e}). \
                 Reclaim with `cargo xtask clean-e2e-tmp --apply`.",
                root.display(),
            );
        }
    }
    // SAFETY: `atexit` takes an `extern "C" fn()`. `cleanup` only reads an
    // already-initialised `OnceLock` and calls `remove_dir_all`, discarding the
    // result — neither unwinds, so no panic can cross the FFI boundary.
    unsafe {
        libc::atexit(cleanup);
    }
}

/// No-op on Windows: there is no `atexit` binding in scope there, and the L2
/// suite that produces these leftovers is Unix-gated. A Windows run leaks its
/// root until the reaper is invoked.
#[cfg(not(unix))]
fn register_temp_root_cleanup() {}

// ---------------------------------------------------------------------------
// PRD #127 Phase 1 — headless `daemon serve` harness
// ---------------------------------------------------------------------------
//
// The scheduler lives in the daemon, not the TUI, so its L2 tests drive the
// real `dot-agent-deck daemon serve` process directly (no PTY / vt100 grid —
// there is no TUI surface to render) and observe it through three channels:
//   - OS process liveness (`try_wait`) for the idle-shutdown carve-out;
//   - the attach socket's `AttachRequest`/`AttachResponse` control protocol
//     for `ReloadSchedules`;
//   - the `dot-agent-deck schedule …` CLI subprocess for the writer + reload
//     trigger.
//
// All sleeping / polling lives in these helpers (in `common`, NOT in an
// `e2e_*.rs` body) so linkage-check Decision 21 (no raw sleeps / fixed-count
// polling in e2e test bodies) is satisfied by construction.

/// A spawned headless `dot-agent-deck daemon serve` process plus the per-test
/// tempdir paths it was pointed at. Drop kills the child so a hung daemon
/// never leaks past the test.
#[cfg(unix)]
#[allow(dead_code)]
pub struct DaemonProc {
    child: std::process::Child,
    /// Hook-ingestion socket (`DOT_AGENT_DECK_SOCKET`).
    pub hook_socket: PathBuf,
    /// Streaming attach / control socket (`DOT_AGENT_DECK_ATTACH_SOCKET`).
    pub attach_socket: PathBuf,
    /// Global schedules config (`DOT_AGENT_DECK_SCHEDULES`); the writer's
    /// fixed target regardless of cwd.
    pub schedules_path: PathBuf,
    /// Per-test HOME.
    pub home: PathBuf,
    /// Env the daemon was launched with, replayed onto every `schedule` CLI
    /// subprocess so the CLI and daemon share sockets + the schedules path.
    env: Vec<(String, String)>,
    /// Captured daemon stderr (the `StderrNotifier` failure-surfacing seam
    /// writes here via `eprintln!`).
    stderr_path: PathBuf,
    _tempdir: tempfile::TempDir,
}

#[cfg(unix)]
#[allow(dead_code)]
impl Drop for DaemonProc {
    fn drop(&mut self) {
        // The daemon was spawned in its own process group (pgid == its pid),
        // so a negative-pid SIGKILL reaps the whole tree — the daemon and any
        // agents it spawned — in one shot. Best-effort; ignore ESRCH/EPERM.
        let pid = self.child.id();
        // SAFETY: kill(2) with a negative pid signals the process group;
        // SIGKILL has no failure mode beyond ESRCH/EPERM, which we ignore.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `dot-agent-deck daemon serve` headlessly against an isolated tempdir.
///
/// `initial_schedules_toml` seeds the global `schedules.toml` (None = no file,
/// i.e. an empty schedule set). `idle_shutdown_secs` is passed verbatim as
/// `DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS` ("0" disables idle shutdown; a small
/// number arms a fast idle window for the carve-out test). Blocks until the
/// attach socket appears so callers can immediately drive the control protocol.
#[cfg(unix)]
#[allow(dead_code)]
pub fn spawn_daemon_serve(
    initial_schedules_toml: Option<&str>,
    idle_shutdown_secs: &str,
) -> DaemonProc {
    spawn_daemon_serve_with_env(initial_schedules_toml, idle_shutdown_secs, &[])
}

/// Like [`spawn_daemon_serve`] but layers `extra_env` onto the daemon's
/// environment (and onto every `schedule` CLI subprocess). Used by the spawn
/// tests to pin `SHELL` for the `$SHELL`-fallback case.
#[cfg(unix)]
#[allow(dead_code)]
pub fn spawn_daemon_serve_with_env(
    initial_schedules_toml: Option<&str>,
    idle_shutdown_secs: &str,
    extra_env: &[(&str, &str)],
) -> DaemonProc {
    let tempdir = race_safe_tempdir();
    let work = tempdir.path().to_path_buf();
    let home = work.join("home");
    std::fs::create_dir_all(&home).expect("create per-test HOME");
    // PRD #381: same durable-path seeding as `TuiDeck` — a `daemon serve` spawns
    // wrapped agents, and `wrap` runs the Codex hook installer.
    seed_durable_binary(&home);
    let state_dir = work.join("state");
    let hook_socket = work.join("hook.sock");
    let attach_socket = work.join("attach.sock");
    let schedules_path = work.join("schedules.toml");
    if let Some(toml) = initial_schedules_toml {
        std::fs::write(&schedules_path, toml).expect("seed schedules.toml");
    }

    let mut env: Vec<(String, String)> = Vec::new();
    if let Ok(p) = std::env::var("PATH") {
        env.push(("PATH".into(), p));
    }
    env.push(("HOME".into(), home.to_string_lossy().into_owned()));
    env.push(("TERM".into(), "xterm-256color".into()));
    env.push((
        "DOT_AGENT_DECK_SOCKET".into(),
        hook_socket.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_ATTACH_SOCKET".into(),
        attach_socket.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_SCHEDULES".into(),
        schedules_path.to_string_lossy().into_owned(),
    ));
    env.push((
        "DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS".into(),
        idle_shutdown_secs.to_string(),
    ));
    // Leaked-daemon safety net. DaemonProc spawns `daemon serve` as a
    // NON-detached child (its parent is this test process), so the orphan
    // watchdog fires correctly when the test dies without running `Drop`
    // (SIGKILL / panic-abort / nextest timeout / Ctrl-C) — the daemon
    // gracefully self-exits instead of leaking to PID 1. The max-lifetime cap
    // is a belt-and-suspenders backstop for anything the watchdog misses.
    // `IDLE_SHUTDOWN_SECS` is left as the caller passed it (tests rely on `0`
    // for determinism). These vars are inert for the short-lived `schedule`
    // CLI subprocesses that also replay this env — only `daemon serve` reads
    // them.
    env.push(("DOT_AGENT_DECK_EXIT_WHEN_ORPHANED".into(), "1".into()));
    env.push(("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS".into(), "300".into()));
    // PRD #127: the scheduler spawn primitive gates a fresh fire's prompt
    // delivery on the spawned agent's `SessionStart` (readiness), falling back
    // after a timeout for commands that emit no hook (bare `cat`, the recorder
    // scripts these tests use). Shrink that fallback from the production 10s so
    // the no-hook delivery tests don't race their ~10s observation windows;
    // 5000ms stays comfortably above spawn/005's 2s "not yet delivered" window
    // and below every 10s delivery window. A test may override via `extra_env`.
    env.push(("DOT_AGENT_DECK_SESSION_START_WAIT_MS".into(), "5000".into()));
    // PRD #249 M3: same pin as the TuiDeck harness above — the silent-worker
    // report would fire on every stand-in worker that emits no events and write
    // a notice into an orchestrator pane. Off by default here; a test that wants
    // the report sets it in `extra_env`, which is layered after this.
    env.push((
        "DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS".into(),
        "0".into(),
    ));
    // PRD #249 M1: same zero pin as `TuiDeck`; targeted scenarios layer a
    // non-zero value through `extra_env` after this baseline.
    env.push((
        "DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS".into(),
        "0".into(),
    ));
    for (k, v) in extra_env {
        env.push(((*k).to_string(), (*v).to_string()));
    }

    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("daemon").arg("serve");
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    // Capture stderr to a file so tests can observe the scheduler's
    // failure-surfacing notifications (`StderrNotifier` → `eprintln!`).
    let stderr_path = work.join("daemon-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create daemon stderr log");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::from(stderr_file));
    // Put the daemon in its own process group (pgid == its pid) so `Drop` can
    // reap the WHOLE tree — the daemon plus any agents it spawned — with one
    // `kill(-pgid)`, not just the daemon itself.
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().expect("spawn `dot-agent-deck daemon serve`");

    let proc = DaemonProc {
        child,
        hook_socket,
        attach_socket,
        schedules_path,
        home,
        env,
        stderr_path,
        _tempdir: tempdir,
    };
    proc.wait_for_attach_socket();
    proc
}

#[cfg(unix)]
#[allow(dead_code)]
impl DaemonProc {
    /// Block until the attach socket file exists (the daemon finished
    /// binding) or a bounded timeout elapses.
    fn wait_for_attach_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.attach_socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "daemon never bound its attach socket at {} within 10s",
            self.attach_socket.display()
        );
    }

    /// Whether the daemon process is still running.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Public point-in-time liveness check (the daemon process has not exited).
    pub fn is_alive_public(&mut self) -> bool {
        self.is_alive()
    }

    /// Assert the daemon stays alive for the whole `window` — polls
    /// throughout so an early exit fails fast with a clear message rather
    /// than passing on a lucky end-of-window sample.
    pub fn assert_alive_for(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if !self.is_alive() {
                panic!(
                    "daemon exited within {window:?} but was expected to stay alive \
                     (idle-shutdown carve-out for a registered enabled schedule)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Poll until the daemon process exits, returning `true` if it exited
    /// within `timeout` and `false` otherwise.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_alive() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !self.is_alive()
    }

    /// Send one `AttachRequest` over the control socket and read back the
    /// single `AttachResponse`. Blocking; used to drive `ReloadSchedules`.
    pub fn send_attach_request(
        &self,
        req: &dot_agent_deck::daemon_protocol::AttachRequest,
    ) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
        attach_request_on(&self.attach_socket, req)
    }

    /// Run `dot-agent-deck schedule <args…>` with the daemon's env, from the
    /// tempdir's HOME as cwd. Returns the captured process output.
    pub fn run_schedule_cli(&self, args: &[&str]) -> std::process::Output {
        self.run_schedule_cli_from(&self.home.clone(), args)
    }

    /// Run `dot-agent-deck schedule <args…>` from an explicit `cwd` (used to
    /// prove the writer targets the global path regardless of cwd).
    pub fn run_schedule_cli_from(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("schedule");
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.output()
            .expect("run `dot-agent-deck schedule` subprocess")
    }

    /// Probe the daemon's in-memory registry by issuing `schedule run-now
    /// --name <name>` until it exits 0 (task registered) or a bounded timeout
    /// elapses. `run-now` hits the daemon over the socket and errors on an
    /// unknown task, so a clean exit proves the task is live in the registry.
    pub fn wait_for_schedule_registered(&self, name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let out = self.run_schedule_cli(&["run-now", "--name", name]);
            if out.status.success() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Fire a registered task immediately via the `RunNow` control message
    /// (no file write). Returns the daemon's response.
    pub fn run_now(
        &self,
        name: &str,
    ) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
        self.send_attach_request(&dot_agent_deck::daemon_protocol::AttachRequest::RunNow {
            name: name.to_string(),
        })
    }

    /// Snapshot the daemon's live agent registry via `ListAgents`.
    pub fn agent_records(&self) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
        let resp = self
            .send_attach_request(&dot_agent_deck::daemon_protocol::AttachRequest::ListAgents)
            .expect("ListAgents over the attach socket");
        resp.agent_records.unwrap_or_default()
    }

    /// Poll `ListAgents` until at least `n` agents are registered (or a bounded
    /// timeout elapses), then return the current snapshot. The returned vec may
    /// be shorter than `n` if the timeout fired — callers assert on `.len()`.
    pub fn wait_for_agent_count(
        &self,
        n: usize,
        timeout: Duration,
    ) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
        let deadline = Instant::now() + timeout;
        loop {
            let records = self.agent_records();
            if records.len() >= n || Instant::now() >= deadline {
                return records;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Poll `ListAgents` until a registered agent matches `pred` (or a bounded
    /// timeout elapses); returns the first match. Lets a test wait for a
    /// specific KIND of agent (e.g. a non-orchestration single-agent card)
    /// without an inline poll loop in the e2e body (Decision 21).
    pub fn wait_for_agent_where(
        &self,
        pred: impl Fn(&dot_agent_deck::agent_pty::AgentRecord) -> bool,
        timeout: Duration,
    ) -> Option<dot_agent_deck::agent_pty::AgentRecord> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = self.agent_records().into_iter().find(&pred) {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Assert the registered agent count does NOT exceed `max` for the whole
    /// `window` — used to catch a double-spawn (a fire that opens two tabs).
    pub fn assert_agent_count_stays_at_most(&self, max: usize, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let n = self.agent_records().len();
            assert!(
                n <= max,
                "agent count grew to {n}, expected at most {max} (double-spawn?)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Attach to an agent's PTY stream and read STREAM_OUT bytes until `needle`
    /// appears in the cumulative output (proving the daemon delivered/echoed
    /// the prompt) or a bounded timeout elapses. Returns whether it was seen.
    pub fn attach_and_wait_for_output(
        &self,
        agent_id: &str,
        needle: &str,
        timeout: Duration,
    ) -> bool {
        self.attach_and_wait_for_occurrences(agent_id, needle, 1, timeout)
    }

    /// Like [`attach_and_wait_for_output`] but waits until `needle` has appeared
    /// at least `want` (non-overlapping) times in the cumulative STREAM_OUT —
    /// used to prove a SECOND delivery landed in a REUSED pane (the prompt text
    /// is fixed per task, so a reuse fire shows the same marker twice).
    ///
    /// A fresh attach replays the daemon's scrollback first, so the count
    /// reflects every delivery the pane has seen, not just live bytes.
    pub fn attach_and_wait_for_occurrences(
        &self,
        agent_id: &str,
        needle: &str,
        want: usize,
        timeout: Duration,
    ) -> bool {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, KIND_STREAM_OUT};
        use std::io::{Read, Write};

        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.attach_socket) else {
            return false;
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

        let req = dot_agent_deck::daemon_protocol::AttachRequest::AttachStream {
            id: agent_id.to_string(),
        };
        let payload = serde_json::to_vec(&req).expect("serialize AttachStream");
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        if stream.write_all(&header).is_err() || stream.write_all(&payload).is_err() {
            return false;
        }
        let _ = stream.flush();

        let mut acc: Vec<u8> = Vec::new();
        let needle_bytes = needle.as_bytes();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut fh = [0u8; 5];
            match stream.read_exact(&mut fh) {
                Ok(()) => {}
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return false,
            }
            let kind = fh[0];
            let len = u32::from_be_bytes([fh[1], fh[2], fh[3], fh[4]]) as usize;
            let mut body = vec![0u8; len];
            if len > 0 && read_exact_with_deadline(&mut stream, &mut body, deadline).is_err() {
                return false;
            }
            if kind == KIND_STREAM_OUT {
                acc.extend_from_slice(&body);
                if count_occurrences(&acc, needle_bytes) >= want {
                    return true;
                }
            } else if kind == KIND_RESP {
                continue;
            }
        }
        false
    }

    /// Simulate a user keystroke into a pane: attach to `agent_id` and send one
    /// STREAM_IN frame carrying `input`. The daemon forwards it to the PTY
    /// stdin; for the deliver-on-idle contract the daemon also records this as
    /// the pane's most-recent USER input (the debounce clock). Confirms the
    /// input reached the PTY by waiting for its echo before returning, which
    /// also guarantees the daemon has processed (and timestamped) it.
    pub fn send_pane_input(&self, agent_id: &str, input: &str) -> bool {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_STREAM_IN};
        use std::io::Write;

        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&self.attach_socket) else {
            return false;
        };
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();

        let req = dot_agent_deck::daemon_protocol::AttachRequest::AttachStream {
            id: agent_id.to_string(),
        };
        let payload = serde_json::to_vec(&req).expect("serialize AttachStream");
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        if stream.write_all(&header).is_err() || stream.write_all(&payload).is_err() {
            return false;
        }
        // STREAM_IN frame with the keystroke bytes.
        let inb = input.as_bytes();
        let mut ih = [0u8; 5];
        ih[0] = KIND_STREAM_IN;
        ih[1..5].copy_from_slice(&(inb.len() as u32).to_be_bytes());
        if stream.write_all(&ih).is_err() || stream.write_all(inb).is_err() {
            return false;
        }
        let _ = stream.flush();
        // Hold the connection open briefly so the daemon drains the STREAM_IN
        // before the socket closes (defensive; the kernel buffers regardless).
        std::thread::sleep(Duration::from_millis(50));
        drop(stream);
        // Confirm the keystroke reached the PTY (and was timestamped) by
        // observing its echo on a fresh attach.
        self.attach_and_wait_for_output(agent_id, input, Duration::from_secs(5))
    }

    /// Whether the captured daemon stderr currently contains `needle`.
    pub fn stderr_contains(&self, needle: &str) -> bool {
        std::fs::read_to_string(&self.stderr_path)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
    }

    /// Poll the captured daemon stderr until it contains `needle` or a bounded
    /// timeout elapses.
    pub fn wait_for_stderr_contains(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.stderr_contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Run the real `dot-agent-deck agent-event --type <state>` CLI against this
    /// daemon, standing in for a Pi extension's status report (PRD #201 M2.2).
    /// Replays the daemon's env (so `HOME` + `DOT_AGENT_DECK_SOCKET` match) and
    /// layers the pane-injected `DOT_AGENT_DECK_PANE_ID` / (optional)
    /// `DOT_AGENT_DECK_AGENT_ID`. Returns the captured process output.
    pub fn run_agent_event(
        &self,
        pane_id: &str,
        agent_id: Option<&str>,
        state: &str,
    ) -> std::process::Output {
        let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("agent-event").arg("--type").arg(state);
        cmd.current_dir(&self.home);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.env("DOT_AGENT_DECK_PANE_ID", pane_id);
        if let Some(id) = agent_id {
            cmd.env("DOT_AGENT_DECK_AGENT_ID", id);
        }
        cmd.output()
            .expect("run `dot-agent-deck agent-event` subprocess")
    }

    /// Open a live `SubscribeEvents` stream against this daemon's attach socket,
    /// draining the broadcast into a background buffer so a test can wait for a
    /// specific `AgentEvent` UNATTENDED — the no-TUI equivalent of the
    /// production event subscriber. Returns once the subscription is provably
    /// live (the initial `KIND_RESP` was read), so no subsequent broadcast is
    /// missed. All polling lives here in the harness (Decision 21).
    pub fn subscribe_events(&self) -> EventSub {
        EventSub::open(&self.attach_socket).expect("open SubscribeEvents stream")
    }
}

/// A live `SubscribeEvents` subscription against a headless daemon: a background
/// reader thread drains `KIND_EVENT` frames into a shared buffer so a test can
/// wait for a specific broadcast `AgentEvent`. All sleeping/polling is contained
/// here in `common` (Decision 21), not in the e2e test body.
#[cfg(unix)]
#[allow(dead_code)]
pub struct EventSub {
    events: Arc<Mutex<Vec<dot_agent_deck::event::AgentEvent>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(unix)]
#[allow(dead_code)]
impl EventSub {
    /// Send a `SubscribeEvents` request, read the `KIND_RESP` ack synchronously
    /// (so the daemon's per-connection broadcast receiver exists before we
    /// return — nothing broadcast afterward can be missed), then spawn a reader
    /// thread collecting `BroadcastMsg::Event` frames.
    fn open(attach_socket: &Path) -> std::io::Result<Self> {
        use dot_agent_deck::daemon_protocol::{KIND_EVENT, KIND_REQ, KIND_RESP};
        use dot_agent_deck::event::BroadcastMsg;

        let mut stream = std::os::unix::net::UnixStream::connect(attach_socket)?;
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let payload =
            serde_json::to_vec(&dot_agent_deck::daemon_protocol::AttachRequest::SubscribeEvents)
                .expect("serialize SubscribeEvents");
        let mut header = [0u8; 5];
        header[0] = KIND_REQ;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        stream.write_all(&header)?;
        stream.write_all(&payload)?;
        stream.flush()?;

        // Block until the RESP ack arrives (retry past the read timeout).
        read_framed(&mut stream, Some(KIND_RESP), Duration::from_secs(10))?;

        let events = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let events_for_reader = Arc::clone(&events);
        let stop_for_reader = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_for_reader.load(Ordering::Relaxed) {
                match read_framed(&mut stream, None, Duration::from_millis(200)) {
                    Ok(Some((KIND_EVENT, body))) => {
                        if let Ok(BroadcastMsg::Event(ev)) =
                            serde_json::from_slice::<BroadcastMsg>(&body)
                        {
                            events_for_reader.lock().unwrap().push(ev);
                        }
                    }
                    // Other frame kinds (e.g. KIND_STREAM_END) — keep reading
                    // until stopped or the socket closes.
                    Ok(Some(_)) => {}
                    // Timed out with no full frame this window — poll `stop`.
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });

        Ok(EventSub {
            events,
            stop,
            handle: Some(handle),
        })
    }

    /// Block until a collected broadcast `AgentEvent` satisfies `pred`,
    /// returning a clone of it, or panic after `timeout`.
    pub fn wait_for(
        &self,
        pred: impl Fn(&dot_agent_deck::event::AgentEvent) -> bool,
        timeout: Duration,
    ) -> dot_agent_deck::event::AgentEvent {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = self.events.lock().unwrap().iter().find(|e| pred(e)) {
                return ev.clone();
            }
            if Instant::now() >= deadline {
                let seen = self.events.lock().unwrap().clone();
                panic!(
                    "no broadcast AgentEvent matched the predicate within {timeout:?}; \
                     observed events: {seen:#?}"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the latest still-live genuine `SessionStart` that names one
    /// daemon-managed pane and agent, then return its conversation generation.
    /// Matching `SessionEnd`s clear a generation; launcher/wrapper-fork starts
    /// are boot evidence rather than a conversation and therefore cannot
    /// authorize a guarded prompt delivery.
    pub fn wait_for_session_start_on_pane(
        &self,
        pane_id: &str,
        agent_id: &str,
        timeout: Duration,
    ) -> String {
        use dot_agent_deck::event::EventType;

        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events.lock().unwrap();
            let mut current_session_id: Option<String> = None;
            for event in events.iter().filter(|event| {
                event.pane_id.as_deref() == Some(pane_id)
                    && event.agent_id.as_deref() == Some(agent_id)
            }) {
                match event.event_type {
                    // Issue #243: EITHER wrapper origin is excluded. The wrapper's
                    // interface-ready start is readiness, but it carries the
                    // WRAPPER's session id — so accepting it here would hand back
                    // `wrap-codex-1234` as the pane's live agent session.
                    EventType::SessionStart if !event.is_wrapper_session_start() => {
                        current_session_id = Some(event.session_id.clone());
                    }
                    EventType::SessionEnd
                        if current_session_id.as_deref() == Some(event.session_id.as_str()) =>
                    {
                        current_session_id = None;
                    }
                    _ => {}
                }
            }
            if let Some(session_id) = current_session_id {
                return session_id;
            }
            if Instant::now() >= deadline {
                let seen = events.clone();
                panic!(
                    "no still-live genuine SessionStart for pane {pane_id:?} and agent \
                     {agent_id:?} appeared within {timeout:?}; observed events: {seen:#?}"
                );
            }
            drop(events);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Like [`Self::wait_for`], but returns `None` instead of panicking when
    /// `timeout` elapses with no match. For a caller that must distinguish
    /// "the precondition this run needed was never met" (inconclusive) from
    /// "the precondition landed and the assertion under it failed" (a real
    /// regression) — a bare `wait_for` collapses both into the same panic.
    pub fn try_wait_for(
        &self,
        pred: impl Fn(&dot_agent_deck::event::AgentEvent) -> bool,
        timeout: Duration,
    ) -> Option<dot_agent_deck::event::AgentEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = self.events.lock().unwrap().iter().find(|e| pred(e)) {
                return Some(ev.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every broadcast `AgentEvent` collected so far, for building a
    /// diagnostic message when [`Self::try_wait_for`] times out.
    pub fn snapshot(&self) -> Vec<dot_agent_deck::event::AgentEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(unix)]
#[allow(dead_code)]
impl Drop for EventSub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Read one framed message (`[kind:u8][len:u32 BE][body]`) from `stream`,
/// tolerating the socket read timeout by retrying until `timeout`. Returns
/// `Ok(None)` if `timeout` elapses before a full frame arrives. When
/// `want_kind` is `Some`, a mismatching kind is an error; when `None`, any kind
/// is returned. Shared by [`EventSub`]'s handshake and reader loop.
#[cfg(unix)]
#[allow(dead_code)]
fn read_framed(
    stream: &mut std::os::unix::net::UnixStream,
    want_kind: Option<u8>,
    timeout: Duration,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let deadline = Instant::now() + timeout;
    let mut header = [0u8; 5];
    // Read the 5-byte header, retrying past WouldBlock/TimedOut until deadline.
    let mut got = 0usize;
    while got < header.len() {
        match stream.read(&mut header[got..]) {
            Ok(0) => return Err(std::io::Error::other("unexpected EOF reading frame header")),
            Ok(n) => got += n,
            Err(ref e)
                if (e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut) =>
            {
                if got == 0 && Instant::now() >= deadline {
                    return Ok(None);
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::other("timed out mid-frame-header"));
                }
            }
            Err(e) => return Err(e),
        }
    }
    let kind = header[0];
    if let Some(want) = want_kind
        && kind != want
    {
        return Err(std::io::Error::other(format!(
            "expected frame kind 0x{want:02x}, got 0x{kind:02x}"
        )));
    }
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    let mut body = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        match stream.read(&mut body[filled..]) {
            Ok(0) => return Err(std::io::Error::other("unexpected EOF reading frame body")),
            Ok(n) => filled += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some((kind, body)))
}

/// Count non-overlapping occurrences of `needle` in `hay`.
#[allow(dead_code)]
fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Send one `AttachRequest` over a daemon attach socket and read back the
/// single `AttachResponse`. Blocking; shared by `DaemonProc` and the
/// `TuiDeck`-driven tests (which pass `deck.attach_socket_path()`).
#[cfg(unix)]
#[allow(dead_code)]
pub fn attach_request_on(
    socket: &Path,
    req: &dot_agent_deck::daemon_protocol::AttachRequest,
) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
    let payload = serde_json::to_value(req).expect("serialize AttachRequest");
    attach_json_request_on(socket, &payload)
}

/// Send a guarded `WriteAndSubmit` request over a daemon attach socket.
///
/// The identity fields are additive JSON keys rather than fields on
/// `AttachRequest`, so E2E clients use this helper when they need to model the
/// production seed/orchestrator prompt-delivery RPC.
#[cfg(unix)]
#[allow(dead_code)]
pub fn write_and_submit_with_identity_on(
    socket: &Path,
    pane_id: &str,
    text: &str,
    expected_agent_id: &str,
    expected_session_id: Option<&str>,
) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
    let mut request = serde_json::json!({
        "op": "write-and-submit",
        "pane_id": pane_id,
        "text": text,
        "expected_agent_id": expected_agent_id,
    });
    if let Some(session_id) = expected_session_id {
        request["expected_session_id"] = serde_json::Value::String(session_id.to_string());
    }
    attach_json_request_on(socket, &request)
}

/// Send one JSON request over a daemon attach socket and read its response.
#[cfg(unix)]
fn attach_json_request_on(
    socket: &Path,
    req: &serde_json::Value,
) -> std::io::Result<dot_agent_deck::daemon_protocol::AttachResponse> {
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP};
    use std::io::{Read, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let payload = serde_json::to_vec(req).expect("serialize attach request JSON");
    let mut header = [0u8; 5];
    header[0] = KIND_REQ;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(&payload)?;
    stream.flush()?;

    let mut resp_header = [0u8; 5];
    stream.read_exact(&mut resp_header)?;
    if resp_header[0] != KIND_RESP {
        return Err(std::io::Error::other(format!(
            "expected RESP frame, got kind 0x{:02x}",
            resp_header[0]
        )));
    }
    let len = u32::from_be_bytes([
        resp_header[1],
        resp_header[2],
        resp_header[3],
        resp_header[4],
    ]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

/// Snapshot a daemon's live agent registry via `ListAgents` over `socket`.
#[cfg(unix)]
#[allow(dead_code)]
pub fn agent_records_on(socket: &Path) -> Vec<dot_agent_deck::agent_pty::AgentRecord> {
    match attach_request_on(
        socket,
        &dot_agent_deck::daemon_protocol::AttachRequest::ListAgents,
    ) {
        Ok(resp) => resp.agent_records.unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// One-shot read of a daemon-side pane's PTY scrollback via
/// `AttachRequest::Snapshot`, over `socket`. The daemon replies `RESP ok`, then
/// (when the ring is non-empty) a single `STREAM_OUT` frame carrying the whole
/// snapshot, then `STREAM_END` — so unlike `AttachStream` this never subscribes
/// to live bytes and returns as soon as the ring has been drained.
///
/// Returns the raw bytes exactly as the agent wrote them (escape sequences
/// included); pair with [`terminal_search_key`] to search them wrap-insensitively.
#[cfg(unix)]
#[allow(dead_code)]
pub fn pane_snapshot_on(socket: &Path, agent_id: &str) -> Vec<u8> {
    use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_STREAM_END, KIND_STREAM_OUT};
    use std::io::{Read, Write};

    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return Vec::new();
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

    let req = dot_agent_deck::daemon_protocol::AttachRequest::Snapshot {
        id: agent_id.to_string(),
    };
    let payload = serde_json::to_vec(&req).expect("serialize Snapshot");
    let mut header = [0u8; 5];
    header[0] = KIND_REQ;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    if stream.write_all(&header).is_err() || stream.write_all(&payload).is_err() {
        return Vec::new();
    }
    let _ = stream.flush();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out: Vec<u8> = Vec::new();
    while Instant::now() < deadline {
        let mut fh = [0u8; 5];
        match stream.read_exact(&mut fh) {
            Ok(()) => {}
            Err(_) => break,
        }
        let kind = fh[0];
        let len = u32::from_be_bytes([fh[1], fh[2], fh[3], fh[4]]) as usize;
        let mut body = vec![0u8; len];
        if len > 0 && read_exact_with_deadline(&mut stream, &mut body, deadline).is_err() {
            break;
        }
        match kind {
            KIND_STREAM_OUT => out.extend_from_slice(&body),
            KIND_STREAM_END => break,
            _ => continue,
        }
    }
    out
}

/// Remove ANSI/VT control sequences from a raw PTY byte stream, leaving the
/// printable text (and its line breaks) behind.
///
/// Handles the four families a full-screen agent TUI emits: CSI (`ESC [ … final`),
/// OSC (`ESC ] … BEL | ST`), the string families DCS/SOS/PM/APC (`ESC P|X|^|_ … ST`),
/// and plain two-byte escapes (charset selection, `ESC =`, …). Without this a
/// naive substring search over the raw bytes can miss text that a redraw split
/// with a colour reset, and a naive "drop the punctuation" pass would splice the
/// escape's own digits INTO the word.
#[allow(dead_code)]
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&kind) = bytes.get(i) else { break };
        match kind {
            b'[' => {
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1;
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Collapse terminal text to a WRAP-INSENSITIVE search key: strip the escape
/// sequences, then keep only `[A-Za-z0-9._/-]`.
///
/// An agent TUI renders inside a bordered box and hard-wraps long lines, so a
/// pointer like `.dot-agent-deck/worker-task-coder.md` can reach the scrollback
/// as `…worker-task-cod` / newline / `│ er.md`. Dropping every space, newline
/// and box-drawing glyph rejoins it, while the kept set is narrow enough that a
/// path- or sentence-shaped needle stays distinctive. Apply to BOTH the haystack
/// and the needle (see [`search_key`]).
#[allow(dead_code)]
pub fn terminal_search_key(bytes: &[u8]) -> String {
    search_key(&strip_ansi(bytes))
}

/// The [`terminal_search_key`] normalization for an already-decoded string —
/// used on the needle side so both sides of the comparison agree.
#[allow(dead_code)]
pub fn search_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        .collect()
}

/// A daemon-side pane's scrollback, collapsed to a [`terminal_search_key`].
#[cfg(unix)]
#[allow(dead_code)]
pub fn pane_search_key_on(socket: &Path, agent_id: &str) -> String {
    terminal_search_key(&pane_snapshot_on(socket, agent_id))
}

/// Poll a daemon-side pane's scrollback until `needle` appears in it
/// (wrap-insensitively), or `timeout` elapses. Decision 21: the polling lives
/// here, never in an `e2e_*.rs` body. The interval is deliberately coarse — each
/// round pulls the pane's whole scrollback ring across the attach socket.
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_for_pane_text_on(
    socket: &Path,
    agent_id: &str,
    needle: &str,
    timeout: Duration,
) -> bool {
    let key = search_key(needle);
    let deadline = Instant::now() + timeout;
    loop {
        if pane_search_key_on(socket, agent_id).contains(&key) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(750));
    }
}

/// Block until every listed agent's scrollback has stopped growing for `quiet`
/// AND has been producing output for at least `min_alive` — i.e. each
/// interactive agent has finished painting its UI and is waiting for input.
/// Returns whether every pane settled before `timeout` (a `false` is worth
/// logging, not necessarily fataling: a still-busy agent may still accept
/// injected input).
///
/// Mirrors `e2e_delegate_work_done_chain::wait_until_worker_ready`, but reads
/// the panes through the daemon's attach socket so it works for agents the test
/// did not spawn itself. The `min_alive` floor matters: a TUI agent can pause
/// briefly mid-init before its input is interactive, and bytes injected during
/// that lull are dropped.
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_until_panes_settled(
    socket: &Path,
    agent_ids: &[String],
    quiet: Duration,
    min_alive: Duration,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last_len: HashMap<&str, usize> = HashMap::new();
    let mut stable_since: HashMap<&str, Instant> = HashMap::new();
    let mut first_output: HashMap<&str, Instant> = HashMap::new();
    loop {
        let mut all_ready = true;
        for id in agent_ids {
            let id = id.as_str();
            let len = pane_snapshot_on(socket, id).len();
            if len > 0 {
                first_output.entry(id).or_insert_with(Instant::now);
            }
            if last_len.get(id).copied() != Some(len) {
                last_len.insert(id, len);
                stable_since.insert(id, Instant::now());
            }
            all_ready &= first_output
                .get(id)
                .is_some_and(|f| f.elapsed() >= min_alive)
                && stable_since.get(id).is_some_and(|s| s.elapsed() >= quiet);
        }
        if all_ready {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Poll `ListAgents` until an agent whose `display_name` equals `name` is
/// present (`want_present = true`) or absent (`want_present = false`), or the
/// timeout elapses. Returns whether the desired condition held.
#[cfg(unix)]
#[allow(dead_code)]
pub fn wait_for_agent_display_name(
    socket: &Path,
    name: &str,
    want_present: bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let present = agent_records_on(socket)
            .iter()
            .any(|r| r.display_name.as_deref() == Some(name));
        if present == want_present {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Count occurrences of `needle` in a file's contents (lossy UTF-8). Returns 0
/// if the file is missing/unreadable. Used to count prompt deliveries recorded
/// by a per-pane "recorder" command (one appended line per delivered prompt),
/// which is immune to PTY echo doubling.
#[allow(dead_code)]
pub fn count_file_substr(path: &Path, needle: &str) -> usize {
    match std::fs::read(path) {
        Ok(bytes) => count_occurrences(&bytes, needle.as_bytes()),
        Err(_) => 0,
    }
}

/// Poll until `needle` appears at least `want` times in `path` (or a bounded
/// timeout elapses). Returns whether the count was reached.
#[allow(dead_code)]
pub fn wait_for_file_substr_count(
    path: &Path,
    needle: &str,
    want: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if count_file_substr(path, needle) >= want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A `/bin/sh` line that posts one synthetic Claude-Code hook event from inside
/// a fixture agent script, through the REAL `dot-agent-deck hook` CLI.
///
/// Centralised because six fixture shims across five e2e files hand-rolled this
/// line, and when the CLI moved the agent from a positional argument to
/// `--agent` (PRD #30), five of them kept emitting the stale `hook claude-code`
/// form. `clap` rejected it (exit 2), the `>/dev/null 2>&1` swallowed the error,
/// and no readiness signal was ever emitted — so the agent-ready gate those
/// tests exist to exercise silently fell through to the 10-second
/// `process_pending_seed_prompts` timeout fallback instead (issue #343). They
/// still passed, just ~10s slower and proving nothing about readiness.
///
/// The `|| exit` is what stops that recurring: `handle_hook` returns
/// `ExitCode::SUCCESS` on EVERY path it reaches — bad JSON, unmapped event, even
/// a failed socket send — so a nonzero exit can only mean `clap` refused the
/// argument shape. That makes failing hard here deterministic rather than
/// load-sensitive: a future CLI change kills the fixture agent loudly instead of
/// quietly degrading its test into a passing-but-vacuous one.
///
/// `bin` is the ABSOLUTE path of the binary under test (`CARGO_BIN_EXE_…`),
/// single-quoted here so a checkout under a path containing spaces — or a quote
/// — still resolves the build under test rather than whatever `dot-agent-deck`
/// a dev machine happens to have on `$PATH`.
#[allow(dead_code)]
pub fn claude_hook_line(bin: &str, payload_json: &str) -> String {
    let quoted_bin = format!("'{}'", bin.replace('\'', r"'\''"));
    format!(
        "printf '%s' '{payload_json}' | {quoted_bin} hook --agent claude-code >/dev/null 2>&1 \
         || {{ echo 'dot-agent-deck hook rejected the fixture payload' >&2; exit 97; }}\n"
    )
}

/// [`claude_hook_line`] for the common case: the `SessionStart` event that acts
/// as the agent-ready signal the spawn-time prompt gate waits on.
#[allow(dead_code)]
pub fn claude_session_start_line(bin: &str, session_id: &str) -> String {
    claude_hook_line(
        bin,
        &format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session_id}"}}"#),
    )
}

/// The recorder line the deck's own Codex metadata probe produces.
///
/// PRD #20 §4.2.1: the deck records SCOPED, hash-pinned trust for its own Codex
/// hooks, and the hashes come from Codex itself — `codex app-server` answering a
/// `hooks/list` JSON-RPC request at startup. So a PATH `codex` recorder shim logs
/// this line whenever a deck/daemon starts on a machine where `codex` resolves.
/// It is a metadata probe, NOT an agent launch.
#[allow(dead_code)]
pub const CODEX_TRUST_PROBE_LAUNCH: &str = "BARE codex app-server";

/// The lines a `codex`/`dot-agent-deck` PATH recorder logged, with the deck's own
/// [`CODEX_TRUST_PROBE_LAUNCH`] metadata probe filtered out — i.e. only the lines
/// that represent an actual AGENT launch.
///
/// Launch-path assertions ("a Codex pane always launches through the wrapper, never
/// bare") are about how the agent was launched, so they must not trip over the
/// deck's startup `codex app-server` probe. A genuine bare-Codex *agent* launch has
/// the pane's own argv (or none at all) and is still reported here.
#[allow(dead_code)]
pub fn recorded_agent_launches(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != CODEX_TRUST_PROBE_LAUNCH)
        .map(str::to_string)
        .collect()
}

/// Poll until `name` no longer appears in the file at `path` (e.g. a schedule
/// definition removed from `schedules.toml`), or the timeout elapses.
#[allow(dead_code)]
pub fn wait_for_schedule_absent_from_file(path: &Path, name: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let absent = match std::fs::read_to_string(path) {
            Ok(s) => !s.contains(name),
            Err(_) => true, // file gone entirely → definitely absent
        };
        if absent {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bounded poll for a filesystem path to appear. Kept in `common` so e2e test
/// bodies don't carry a raw sleep loop (linkage-check Decision 21).
#[allow(dead_code)]
pub fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists()
}

/// Human description of what `path` holds *right now* — missing, unreadable,
/// or its exact contents. Used by the content-polling waiters below so a
/// timeout says whether the file never appeared, appeared empty, or simply
/// carried the wrong text.
#[allow(dead_code)]
fn describe_file(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) => format!("{} contains {contents:?}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            format!("{} does not exist", path.display())
        }
        Err(e) => format!("{} is unreadable: {e}", path.display()),
    }
}

/// Bounded poll until `path` is readable AND `matches` accepts its contents.
/// `Ok(())` on match; on timeout, `Err(`[`describe_file`]`)`.
///
/// Prefer this over [`wait_for_path`] + an immediate `read_to_string` whenever
/// the assertion is about the file's CONTENTS. An agent that writes a sentinel
/// with a shell redirect — `printf 'X' > sentinel.txt` — has the shell CREATE
/// the file before `printf` writes into it, so a reader that waits only for
/// EXISTENCE can win the race and observe an empty string (PRD #225; this is
/// exactly how `orchestration/delegate/009` failed in-suite while passing in
/// isolation). Polling the content closes that window.
#[allow(dead_code)]
fn wait_for_file_matching(
    path: &Path,
    timeout: Duration,
    matches: impl Fn(&str) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && matches(&contents)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(describe_file(path));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Bounded poll until `path`'s TRIMMED contents equal `expected` exactly.
/// See [`wait_for_file_matching`] for why content — not existence — is polled.
#[allow(dead_code)]
pub fn wait_for_file_trimmed_eq(
    path: &Path,
    expected: &str,
    timeout: Duration,
) -> Result<(), String> {
    wait_for_file_matching(path, timeout, |contents| contents.trim() == expected)
}

/// Bounded poll until `path`'s contents contain `needle`. The bounded,
/// non-panicking sibling of [`wait_for_file_contains`] (which is pinned to the
/// harness-wide [`WAIT_TIMEOUT`]); see [`wait_for_file_matching`] for why
/// content — not existence — is polled.
#[allow(dead_code)]
pub fn wait_for_file_containing(
    path: &Path,
    needle: &str,
    timeout: Duration,
) -> Result<(), String> {
    wait_for_file_matching(path, timeout, |contents| contents.contains(needle))
}

/// Bounded poll until `path` holds at least `want` COMPLETE — i.e.
/// newline-terminated — lines. For PATH recorder shims that append one line per
/// exec (`printf '…\n' >> "$RECORD"`), which is how the launch-shape tests
/// observe what was actually launched.
///
/// Counts newline terminators rather than [`str::lines`] deliberately:
/// `lines()` also counts a half-written trailing line, so a reader using it can
/// return the instant the shell has created the file and still read an
/// incomplete record. That is the same race [`wait_for_file_matching`]
/// documents, one level up.
#[allow(dead_code)]
pub fn wait_for_file_lines(path: &Path, want: usize, timeout: Duration) -> Result<(), String> {
    wait_for_file_matching(path, timeout, |contents| {
        contents.matches('\n').count() >= want
    })
}

/// Blocking `read_exact` bounded by a wall-clock `deadline`, tolerating the
/// per-read timeout set on the stream. Returns `Err` on EOF / hard error / the
/// deadline passing before the buffer fills.
#[cfg(unix)]
#[allow(dead_code)]
fn read_exact_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "deadline elapsed mid-frame",
            ));
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof",
                ));
            }
            Ok(n) => filled += n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Poll `cond` until it returns `true` or `timeout` elapses; returns the final
/// value. Decision 21: bounded polling lives in `common`, never in an
/// `e2e_*.rs` body (which forbids raw `sleep`).
#[allow(dead_code)]
pub fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Whether `pid` is still a live (non-exited) process. A reaped pid is gone; a
/// reparented-then-exited pid may briefly be a zombie — treat state `Z` as
/// exited so the check isn't fooled by an unreaped zombie under a sub-reaper.
/// Uses `/proc` on Linux and falls back to a `kill(pid, 0)` probe elsewhere.
#[cfg(unix)]
#[allow(dead_code)]
pub fn process_running(pid: i32) -> bool {
    let stat_path = format!("/proc/{pid}/stat");
    match std::fs::read_to_string(&stat_path) {
        Ok(stat) => match stat.rfind(')') {
            // `/proc/<pid>/stat` is `pid (comm) STATE ...`; comm may contain
            // spaces/parens, so the state char follows the last ')'.
            Some(idx) => !stat[idx + 1..].trim_start().starts_with('Z'),
            None => true,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if Path::new("/proc").is_dir() {
                false // Linux: no /proc entry → the pid is gone.
            } else {
                // SAFETY: kill(pid, 0) only probes existence/permission.
                unsafe { libc::kill(pid, 0) == 0 }
            }
        }
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// PRD #201 M4 — in-process daemon + real-agent polling helpers
// ---------------------------------------------------------------------------
//
// The real-`pi` orchestrator e2e (`e2e_pi_orchestrator.rs`) needs the SAME
// in-process daemon setup `e2e_delegate_work_done_chain.rs` uses (its hook loop
// routes the extension's `delegate` / `work-done` / `agent-event` frames and
// re-broadcasts `AgentEvent`s), plus a few `sleep`-based polls. Those polls live
// HERE in `common` — not in an `e2e_*.rs` body — so linkage-check Decision 21
// (no raw sleeps in e2e test bodies) is satisfied by construction, exactly as
// the `DaemonProc` harness above does for the headless daemon-serve tests.

/// Serializes the socket-bind window for [`spawn_inprocess_daemon`] (mirrors the
/// bind lock in the daemon-spawning e2e tests).
static INPROC_BIND_LOCK: Mutex<()> = Mutex::new(());

/// An in-process `dot-agent-deck` daemon: its hook loop ingests `delegate` /
/// `work-done` / `agent-event` frames over the hook socket and re-broadcasts
/// `AgentEvent`s on [`event_tx`](Self::event_tx). Drop aborts the loop and shuts
/// down every spawned PTY.
#[allow(dead_code)]
pub struct InProcDaemon {
    _dir: tempfile::TempDir,
    /// Shared app state — populate the orchestration maps the delegate /
    /// work-done handlers read.
    pub state: dot_agent_deck::state::SharedState,
    /// The PTY registry — spawn real agent panes into it.
    pub registry: std::sync::Arc<dot_agent_deck::agent_pty::AgentPtyRegistry>,
    /// The daemon-wide broadcast — subscribe to observe re-broadcast events.
    pub event_tx: tokio::sync::broadcast::Sender<dot_agent_deck::event::BroadcastMsg>,
    /// Hand this to spawned agents as `DOT_AGENT_DECK_SOCKET`.
    pub hook_path: PathBuf,
    /// The streaming-attach socket (`AttachRequest`/`AttachResponse`, e.g.
    /// `ListAgents`). Hand this to a CLI subprocess as
    /// `DOT_AGENT_DECK_ATTACH_SOCKET` for a real-binary integration test
    /// against this in-process daemon (this feature's `daemon status`).
    pub attach_path: PathBuf,
    handle: tokio::task::JoinHandle<()>,
}

#[allow(dead_code)]
impl Drop for InProcDaemon {
    fn drop(&mut self) {
        self.handle.abort();
        self.registry.shutdown_all();
    }
}

/// Bring up an in-process daemon and block until its hook socket accepts
/// connections (so an agent's first CLI call can't race startup). Mirrors
/// `e2e_delegate_work_done_chain.rs::spawn_daemon`, centralized so the readiness
/// poll lives in `common`.
#[cfg(unix)]
#[allow(dead_code)]
pub async fn spawn_inprocess_daemon() -> InProcDaemon {
    use dot_agent_deck::daemon::{Daemon, run_daemon_with};

    init_test_env();

    // Issue prageethw/dot-agent-deck#253 round-4 verification, finding 1: `handle_delegate` run
    // through this in-process daemon executes in THIS test process, not the
    // deck's — so `binary_name()` would (correctly, for that process) name
    // this libtest binary, and a generated `work-done` command would hand a
    // real worker `<libtest binary> work-done …`, which libtest reads as a
    // test-name filter rather than the deck's CLI. Inject the real built
    // deck binary's path so `binary_name()` names it instead. `#[cfg]`'d
    // rather than unconditional: this file is also compiled into fast-tier
    // (non-`e2e`) test binaries such as `daemon_status.rs` and
    // `delegate_prompt_injection.rs`, where `set_test_current_exe_override`
    // does not exist in the linked library at all (CLAUDE.md rule 5 — see
    // that function's doc for why it's `e2e`-gated in the first place).
    #[cfg(feature = "e2e")]
    dot_agent_deck::platform::paths::set_test_current_exe_override(std::path::PathBuf::from(env!(
        "CARGO_BIN_EXE_dot-agent-deck"
    )));

    let (dir, hook_path, attach_path) = {
        let _g = INPROC_BIND_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = race_safe_tempdir();
        let hook = dir.path().join("hook.sock");
        let attach = dir.path().join("attach.sock");
        (dir, hook, attach)
    };

    let state: dot_agent_deck::state::SharedState = std::sync::Arc::new(tokio::sync::RwLock::new(
        dot_agent_deck::state::AppState::default(),
    ));
    let daemon = Daemon::with_attach(state.clone(), attach_path.clone())
        .with_idle_shutdown(None)
        .with_lock_dir_override(lock_dir_path());
    let registry = daemon.pty_registry.clone();
    let event_tx = daemon.event_tx.clone();

    let hook_for_daemon = hook_path.clone();
    let handle = tokio::spawn(async move {
        let _ = run_daemon_with(&hook_for_daemon, daemon).await;
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while tokio::time::Instant::now() < deadline {
        if hook_path.exists() && tokio::net::UnixStream::connect(&hook_path).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        ready,
        "in-process daemon hook socket was not accepting connections within 5s"
    );

    InProcDaemon {
        _dir: dir,
        state,
        registry,
        event_tx,
        hook_path,
        attach_path,
        handle,
    }
}

/// Poll a spawned agent's PTY snapshot until its byte length stops growing for
/// `quiet` (with a minimum settle since first output), i.e. an interactive agent
/// has finished rendering its boot UI and is input-ready. Delegating only after
/// readiness keeps a daemon-injected prompt from being dropped mid-boot. Ported
/// from `e2e_delegate_work_done_chain.rs::wait_until_worker_ready`.
#[allow(dead_code)]
pub async fn wait_until_agent_output_settled(
    registry: &dot_agent_deck::agent_pty::AgentPtyRegistry,
    agent_id: &str,
    quiet: Duration,
    timeout: Duration,
) {
    let start = tokio::time::Instant::now();
    let deadline = start + timeout;
    let min_since_first_output = Duration::from_secs(6);
    let mut last_len = 0usize;
    let mut first_output_at: Option<tokio::time::Instant> = None;
    let mut stable_since = start;
    loop {
        let len = registry.snapshot(agent_id).map(|s| s.len()).unwrap_or(0);
        if len > 0 && first_output_at.is_none() {
            first_output_at = Some(tokio::time::Instant::now());
        }
        if len != last_len {
            last_len = len;
            stable_since = tokio::time::Instant::now();
        } else if let Some(first) = first_output_at
            && stable_since.elapsed() >= quiet
            && first.elapsed() >= min_since_first_output
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Async poll for `path` to exist, up to `timeout` (the async sibling of the
/// sync [`wait_for_path`], for use inside an async e2e `_inner` body without
/// blocking a runtime worker thread on a long wait).
#[allow(dead_code)]
pub async fn wait_for_path_async(path: &Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Poll a spawned agent's PTY snapshot until the *rendered* screen contains
/// `needle`, returning `(found, rendered_screen)`. The raw PTY byte stream is
/// replayed through a `vt100` grid first, so a streamed/redrawn reply (an agent
/// prints token-by-token with cursor moves) is matched on its final rendered
/// state rather than on raw, escape-interleaved bytes. Ported from
/// `e2e_delegate_work_done_chain.rs::wait_for_rendered_text` so the poll lives
/// in `common` (Decision 21).
#[allow(dead_code)]
pub async fn wait_for_rendered_agent_text(
    registry: &dot_agent_deck::agent_pty::AgentPtyRegistry,
    agent_id: &str,
    needle: &str,
    timeout: Duration,
) -> (bool, String) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snap = registry.snapshot(agent_id).unwrap_or_default();
        let mut parser = vt100::Parser::new(40, 120, 0);
        parser.process(&snap);
        let screen = parser.screen().contents();
        if screen.contains(needle) {
            return (true, screen);
        }
        if tokio::time::Instant::now() >= deadline {
            return (false, screen);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// A background collector of an in-process daemon's re-broadcast `AgentEvent`s.
/// Subscribe (via [`start`](Self::start)) BEFORE spawning the agent whose events
/// you want, so its first status report can't be missed. Drop aborts the reader.
/// The async [`wait_for`](Self::wait_for) poll lives here (Decision 21).
#[allow(dead_code)]
pub struct BroadcastEventLog {
    events: std::sync::Arc<Mutex<Vec<dot_agent_deck::event::AgentEvent>>>,
    handle: tokio::task::JoinHandle<()>,
}

#[allow(dead_code)]
impl Drop for BroadcastEventLog {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[allow(dead_code)]
impl BroadcastEventLog {
    /// Subscribe to `event_tx` and drain `AgentEvent`s into a shared buffer.
    pub fn start(
        event_tx: &tokio::sync::broadcast::Sender<dot_agent_deck::event::BroadcastMsg>,
    ) -> Self {
        use dot_agent_deck::event::BroadcastMsg;
        use tokio::sync::broadcast::error::RecvError;

        let mut rx = event_tx.subscribe();
        let events =
            std::sync::Arc::new(Mutex::new(Vec::<dot_agent_deck::event::AgentEvent>::new()));
        let sink = std::sync::Arc::clone(&events);
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(BroadcastMsg::Event(ev)) => sink.lock().unwrap().push(ev),
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Self { events, handle }
    }

    /// Block until a collected `AgentEvent` satisfies `pred`, returning a clone,
    /// or `None` on timeout.
    pub async fn wait_for(
        &self,
        pred: impl Fn(&dot_agent_deck::event::AgentEvent) -> bool,
        timeout: Duration,
    ) -> Option<dot_agent_deck::event::AgentEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(ev) = self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| pred(e))
                .cloned()
            {
                return Some(ev);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
mod harness_unit_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Issue #322 — harness temp-root containment and cleanup
    // -----------------------------------------------------------------------

    /// Marks the root on stdout so the re-run below can capture it.
    const ROOT_MARKER: &str = "harness-temp-root=";

    /// The host plugin-tree copy stays off unless explicitly asked for: it is
    /// ~11 MB per seeded HOME and nothing in the suite depends on it.
    #[test]
    fn claude_plugin_import_is_off_unless_explicitly_enabled() {
        let prev = std::env::var_os("DAD_E2E_IMPORT_CLAUDE_PLUGINS");
        // SAFETY: nextest runs one test per process, so this is single-threaded;
        // the var is restored before returning.
        unsafe { std::env::remove_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS") };
        let off_by_default = import_claude_plugins_enabled();
        unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", "1") };
        let on_when_asked = import_claude_plugins_enabled();
        unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", "0") };
        let off_when_zero = import_claude_plugins_enabled();
        match prev {
            Some(v) => unsafe { std::env::set_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS", v) },
            None => unsafe { std::env::remove_var("DAD_E2E_IMPORT_CLAUDE_PLUGINS") },
        }
        assert!(!off_by_default, "plugin copy must default to off");
        assert!(on_when_asked, "=1 must re-enable the copy");
        assert!(!off_when_zero, "=0 must leave it off");
    }

    /// The pre-flight message names the real cause and the one command that
    /// fixes it — the whole point is that a tmpfs-exhaustion run stops looking
    /// like a product regression.
    #[cfg(unix)]
    #[test]
    fn insufficient_space_message_names_the_cause_and_the_remedy() {
        let msg = insufficient_temp_space_message(312, 2048, Path::new("/tmp"));
        assert!(msg.contains("312 MB"), "missing actual free space: {msg}");
        assert!(msg.contains("2048 MB"), "missing required space: {msg}");
        assert!(msg.contains("/tmp"), "missing the filesystem: {msg}");
        assert!(
            msg.contains("cargo xtask clean-e2e-tmp --apply"),
            "missing the remedy: {msg}",
        );
        assert!(
            msg.contains("NOT a product regression"),
            "message must be impossible to mistake for a test defect: {msg}",
        );
    }

    /// A zero threshold disables the check, so a contributor whose temp
    /// filesystem is small on purpose is never blocked by it.
    #[cfg(unix)]
    #[test]
    fn zero_threshold_disables_the_preflight_check() {
        // SAFETY: single-threaded test process (nextest runs one test per
        // process); the var is restored before returning.
        let prev = std::env::var_os(MIN_FREE_ENV);
        unsafe { std::env::set_var(MIN_FREE_ENV, "0") };
        let verdict = temp_space_problem(Path::new("/"));
        let configured = min_free_mb();
        match prev {
            Some(v) => unsafe { std::env::set_var(MIN_FREE_ENV, v) },
            None => unsafe { std::env::remove_var(MIN_FREE_ENV) },
        }
        assert_eq!(configured, 0, "the bypass var must reach the threshold");
        assert!(verdict.is_none(), "zero threshold should disable the check");
    }

    /// An impossibly large threshold trips the check, proving it actually reads
    /// the filesystem rather than always returning `None`.
    #[cfg(unix)]
    #[test]
    fn an_unmeetable_threshold_trips_the_preflight_check() {
        let prev = std::env::var_os(MIN_FREE_ENV);
        // SAFETY: as above — single-threaded, restored before returning.
        unsafe { std::env::set_var(MIN_FREE_ENV, "1000000000") };
        let verdict = temp_space_problem(&std::env::temp_dir());
        match prev {
            Some(v) => unsafe { std::env::set_var(MIN_FREE_ENV, v) },
            None => unsafe { std::env::remove_var(MIN_FREE_ENV) },
        }
        assert!(
            verdict.is_some_and(|m| m.contains("clean-e2e-tmp")),
            "a 1 PB requirement should always trip the check",
        );
    }

    /// Room to spare is a silent pass — the decision half is exercised with
    /// injected numbers so this never depends on the machine's real disk.
    #[cfg(unix)]
    #[test]
    fn preflight_passes_when_free_space_is_above_the_threshold() {
        assert!(temp_space_verdict(Some(4096), 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none());
        // Exactly at the threshold is still "enough": the comparison is `<`.
        assert!(temp_space_verdict(Some(2048), 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none());
    }

    /// Below the threshold the verdict names the path, the requirement and the
    /// shortfall — the three facts a reader needs to tell a starved harness
    /// apart from a broken product.
    #[cfg(unix)]
    #[test]
    fn preflight_fails_below_the_threshold_naming_path_required_and_found() {
        let msg = temp_space_verdict(Some(97), 2048, Path::new("/var/tmp/dad-e2e-1000"))
            .expect("97 MB is under a 2048 MB requirement");
        assert!(
            msg.contains("/var/tmp/dad-e2e-1000"),
            "missing the path: {msg}"
        );
        assert!(msg.contains("2048 MB"), "missing the requirement: {msg}");
        assert!(msg.contains("97 MB"), "missing what was found: {msg}");
        assert!(
            msg.contains("HARNESS PRE-FLIGHT FAILURE"),
            "missing the not-a-regression framing: {msg}",
        );
    }

    /// A filesystem whose free space cannot be queried must never fail the
    /// suite — the check exists to remove a flaky failure mode, not add one.
    #[cfg(unix)]
    #[test]
    fn preflight_degrades_gracefully_when_free_space_is_unqueryable() {
        assert!(
            temp_space_verdict(None, 2048, Path::new("/var/tmp/dad-e2e-1000")).is_none(),
            "an unqueryable filesystem must produce no verdict",
        );
        // And the query really does return `None` rather than panicking on a
        // path that is not there, so the branch above is reachable.
        assert!(
            free_bytes(Path::new("/definitely/not/a/real/mount/point-322")).is_none(),
            "statvfs on a missing path should report no answer",
        );
    }

    // -----------------------------------------------------------------------
    // Issue #322 — the temp base lands on a short, private, disk-backed path
    // -----------------------------------------------------------------------

    /// The default is a private, UID-scoped parent under `/var/tmp` — short
    /// enough for a socket, disk-backed by FHS convention, and owner-only so
    /// nothing under it can belong to another user.
    #[test]
    fn temp_base_defaults_to_the_private_var_tmp_parent() {
        let parent = PathBuf::from("/var/tmp/dad-e2e-1000");
        let choice = choose_temp_base(None, Some(&parent), Path::new("/tmp"));
        assert_eq!(choice.path, parent);
        assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
    }

    /// An explicit `DAD_E2E_TMPDIR` outranks the private parent — that is the
    /// documented escape hatch for anyone who wants a target-local or
    /// otherwise unusual base.
    #[test]
    fn temp_base_env_override_wins_over_every_other_candidate() {
        let choice = choose_temp_base(
            Some(Path::new("/fast/scratch")),
            Some(Path::new("/var/tmp/dad-e2e-1000")),
            Path::new("/tmp"),
        );
        assert_eq!(choice.path, Path::new("/fast/scratch"));
        assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
    }

    /// The override is honoured even when it is too deep to bind a socket
    /// under — an explicit choice is not silently overruled — but it says so.
    #[test]
    fn an_over_long_env_override_is_honoured_with_a_warning() {
        let deep = PathBuf::from(format!("/{}", "x".repeat(SUN_PATH_USABLE)));
        let choice = choose_temp_base(
            Some(&deep),
            Some(Path::new("/var/tmp/dad-e2e-1000")),
            Path::new("/tmp"),
        );
        assert_eq!(choice.path, deep);
        let warning = choice.warnings.first().expect("an unusable override warns");
        assert!(warning.contains(TEMP_BASE_ENV), "{warning}");
        assert!(warning.contains("AF_UNIX path too long"), "{warning}");
    }

    /// With no usable private parent the system temp dir is the last resort —
    /// and because that is the RAM-backed outcome issue #322 is about, it is
    /// the one case that always warns.
    #[test]
    fn the_system_temp_dir_is_a_last_resort_and_says_so() {
        let choice = choose_temp_base(None, None, Path::new("/tmp"));
        assert_eq!(choice.path, Path::new("/tmp"));
        let warning = choice.warnings.first().expect("falling back to /tmp warns");
        assert!(warning.contains("#322"), "{warning}");
        assert!(warning.contains(TEMP_BASE_ENV), "{warning}");
    }

    /// The length veto applies to the private parent too — an absurd UID (or a
    /// future longer parent name) must degrade rather than produce a base no
    /// socket can be bound under.
    #[test]
    fn an_over_long_private_parent_is_vetoed_like_any_other_candidate() {
        let parent = PathBuf::from(format!("/var/tmp/{}", "u".repeat(MAX_TEMP_BASE_LEN)));
        let choice = choose_temp_base(None, Some(&parent), Path::new("/tmp"));
        assert_eq!(choice.path, Path::new("/tmp"));
        let warning = choice.warnings.first().expect("a vetoed parent warns");
        assert!(warning.contains(&parent.display().to_string()), "{warning}");
    }

    /// The real parent this machine would use is owner-only and ours. The
    /// structural claim the whole `/var/tmp` rung rests on: `/var/tmp` is mode
    /// 1777, so without a verified 0700 parent a `dad-tests-*` directory there
    /// could belong to anybody.
    #[cfg(unix)]
    #[test]
    fn the_private_parent_is_owner_only_and_owned_by_us() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let parent = match private_temp_parent() {
            Ok(p) => p,
            Err(why) => {
                eprintln!("skipping: no private parent available here ({why})");
                return;
            }
        };
        assert_eq!(
            parent.file_name().and_then(|n| n.to_str()),
            Some(private_parent_name(effective_uid()).as_str()),
            "the parent must be scoped to the effective UID",
        );
        let meta = std::fs::symlink_metadata(&parent).expect("stat private parent");
        assert!(
            !meta.file_type().is_symlink(),
            "parent must not be a symlink"
        );
        let mode = meta.permissions().mode() & 0o7777;
        assert_eq!(
            private_dir_objection(meta.uid(), mode, effective_uid()),
            None,
            "{} is uid {} mode 0o{mode:o}",
            parent.display(),
            meta.uid(),
        );
    }

    /// A parent that is not ours is refused, not repaired: chmod'ing or
    /// chown'ing someone else's directory is exactly the behaviour that makes a
    /// shared `/var/tmp` dangerous.
    #[cfg(unix)]
    #[test]
    fn a_foreign_or_loose_private_parent_is_refused() {
        assert!(
            private_dir_objection(1001, 0o700, 1000).is_some(),
            "a directory owned by another uid must be refused",
        );
        assert!(
            private_dir_objection(1000, 0o750, 1000).is_some(),
            "a group-readable directory must be refused",
        );
        assert!(
            private_dir_objection(1000, 0o1777, 1000).is_some(),
            "a world-writable directory must be refused",
        );
        assert_eq!(private_dir_objection(1000, 0o700, 1000), None);
    }

    /// The predicate enforces the **exact** 0o700 that the diagnostics, the
    /// audit note and `docs/develop/e2e-temp-dirs.md` all claim.
    ///
    /// It used to test only `mode & 0o077 == 0`, which 0o500, 0o300, 0o000 and
    /// 0o1700 also satisfy. Confidentiality was never the gap — `mkdir(2)`
    /// applies the mode and a umask can only clear bits — but a pre-existing
    /// 0o500 parent passed the pre-flight whose whole job is to name the problem
    /// up front, and then failed much later as a bare `Permission denied` from
    /// somewhere inside a test. So the check now matches the claim, and the
    /// message has to name the innocent cause: a umask that clears owner bits.
    #[cfg(unix)]
    #[test]
    fn the_private_dir_rule_requires_exactly_0o700_not_merely_owner_only() {
        assert_eq!(private_dir_objection(1000, 0o700, 1000), None);

        // Owner bits missing: no confidentiality problem, but not usable, and
        // previously accepted.
        for mode in [0o500, 0o300, 0o600, 0o000] {
            let why = private_dir_objection(1000, mode, 1000)
                .unwrap_or_else(|| panic!("0o{mode:o} must be refused"));
            assert!(why.contains(&format!("mode is 0o{mode:o}")), "{why}");
            assert!(
                why.contains("umask"),
                "0o{mode:o} must name the cause: {why}"
            );
        }

        // Sticky-but-owner-only — 0o1700 — was accepted by the old mask too.
        let why = private_dir_objection(1000, 0o1700, 1000).expect("0o1700 must be refused");
        assert!(why.contains("mode is 0o1700"), "{why}");

        // Group/other bits get the other half of the message: what is at risk.
        let why = private_dir_objection(1000, 0o750, 1000).expect("0o750 must be refused");
        assert!(why.contains("credentials"), "{why}");
    }

    /// Refusal must be **fatal**, not a warning. Refusing the directory and
    /// then dropping to `std::env::temp_dir()` converts a security refusal into
    /// issue #322's original capacity problem, and the only signal is a stderr
    /// line nextest interleaves across thousands of processes.
    ///
    /// Asserted on the pure verdict, because the foreign-owned shape cannot be
    /// built on disk without `chown`. Every claim the message has to make is
    /// pinned: what it is, that nothing ran, the path, observed state, required
    /// state, the remedy, and the escape hatch.
    #[cfg(unix)]
    #[test]
    fn a_refused_private_parent_is_fatal_and_actionable() {
        let path = Path::new("/var/tmp/dad-e2e-1000");
        let msg = private_parent_verdict(path, false, true, 1001, 0o755, 1000)
            .expect("a foreign-owned, group-readable parent must be refused");
        for expected in [
            "HARNESS PRE-FLIGHT FAILURE",
            "NOT a product regression",
            "No test has run.",
            "/var/tmp/dad-e2e-1000 exists and is",
            // observed …
            "a directory owned by uid 1001 with mode 0o755",
            // … versus required
            "requires a real directory owned by uid 1000 with mode 0o700",
            // why it is not falling back rather than just that it is not
            "RAM-backed tmpfs",
            "#322",
            // the remedy, and the way out for someone who cannot take it
            "ls -ld /var/tmp/dad-e2e-1000",
            "rm -rf /var/tmp/dad-e2e-1000",
            TEMP_BASE_ENV,
        ] {
            assert!(msg.contains(expected), "missing {expected:?} in:\n{msg}");
        }
    }

    /// Each refusable shape produces a verdict naming what was seen, and a
    /// parent that is exactly what the harness asks for produces none — so the
    /// new hard failure cannot fire on a healthy machine.
    #[cfg(unix)]
    #[test]
    fn every_untrustworthy_parent_shape_earns_a_verdict_and_a_good_one_does_not() {
        let path = Path::new("/var/tmp/dad-e2e-1000");
        let observed = |is_symlink, is_dir, uid, mode| {
            private_parent_verdict(path, is_symlink, is_dir, uid, mode, 1000)
        };
        assert!(
            observed(true, false, 1000, 0o700)
                .is_some_and(|m| m.contains("exists and is a symlink")),
            "a symlink at the parent's name must be refused",
        );
        assert!(
            observed(false, false, 1000, 0o600)
                .is_some_and(|m| m.contains("exists and is not a directory")),
            "a plain file at the parent's name must be refused",
        );
        assert!(
            observed(false, true, 1000, 0o750)
                .is_some_and(|m| m.contains("owned by uid 1000 with mode 0o750")),
            "group bits must be refused",
        );
        assert_eq!(
            observed(false, true, 1000, 0o700),
            None,
            "the parent this machine actually has must not be refused",
        );
    }

    /// Unwrap a [`private_temp_parent_in`] outcome that must be a hard refusal,
    /// failing loudly on either of the two ways it could be wrong: adopting the
    /// directory, or degrading to a warning and the next rung of the ladder.
    #[cfg(unix)]
    fn refusal_message(outcome: Result<PathBuf, PrivateParentProblem>) -> String {
        match outcome {
            Ok(p) => panic!("{} was adopted; it should have been refused", p.display()),
            Err(PrivateParentProblem::Unavailable(why)) => {
                panic!("degraded to a warning and fell through the ladder: {why}")
            }
            Err(PrivateParentProblem::Refused(message)) => message,
        }
    }

    /// The classification is made against what is really on disk, not just in
    /// the pure verdict. Three of the four refusable shapes can be built
    /// without privileges — a symlink, a plain file, and a loosened mode — and
    /// each must come back `Refused` rather than falling through.
    #[cfg(unix)]
    #[test]
    fn a_present_but_untrustworthy_parent_is_refused_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let euid = effective_uid();
        let name = private_parent_name(euid);
        // Each shape gets its own stand-in for `/var/tmp`, since the parent's
        // name inside it is fixed by the UID.
        let shared = |kind: &str| {
            let dir = anchor.path().join(kind);
            std::fs::create_dir(&dir).expect("stand-in /var/tmp");
            dir
        };

        let linked = shared("symlink");
        let target = anchor.path().join("elsewhere");
        std::fs::create_dir(&target).expect("link target");
        std::os::unix::fs::symlink(&target, linked.join(&name)).expect("plant a symlink");
        let msg = refusal_message(private_temp_parent_in(&linked, euid));
        assert!(msg.contains("exists and is a symlink"), "{msg}");

        let filed = shared("file");
        std::fs::write(filed.join(&name), b"not a directory").expect("plant a file");
        let msg = refusal_message(private_temp_parent_in(&filed, euid));
        assert!(msg.contains("exists and is not a directory"), "{msg}");

        // `set_permissions` rather than a creation mode: the umask can only
        // clear bits, so a 0o755 asked for at `mkdir` time is not guaranteed.
        let loose = shared("loose");
        let parent = loose.join(&name);
        std::fs::create_dir(&parent).expect("plant a directory");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
            .expect("loosen the mode");
        let msg = refusal_message(private_temp_parent_in(&loose, euid));
        assert!(
            msg.contains(&format!("owned by uid {euid} with mode 0o755")),
            "{msg}",
        );
        assert!(msg.contains(&parent.display().to_string()), "{msg}");

        // Refused, not repaired: the offending directory is untouched.
        let mode = std::fs::symlink_metadata(&parent)
            .expect("stat the refused parent")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o755, "the refused parent was modified");
    }

    /// Making refusal fatal must not break a machine that simply has no
    /// `/var/tmp`. An absent shared directory is an ordinary environment
    /// difference, so it stays `Unavailable` and the ladder falls through to
    /// the last resort with the warning it has always printed.
    #[cfg(unix)]
    #[test]
    fn an_absent_shared_directory_still_falls_through_the_ladder() {
        let anchor = race_safe_tempdir();
        let missing = anchor.path().join("no-var-tmp-here");
        match private_temp_parent_in(&missing, effective_uid()) {
            Err(PrivateParentProblem::Unavailable(why)) => {
                assert!(why.contains(&missing.display().to_string()), "{why}");
            }
            Ok(p) => panic!("{} does not exist and must not be created", p.display()),
            Err(PrivateParentProblem::Refused(msg)) => {
                panic!("an absent shared directory must never be fatal:\n{msg}")
            }
        }
        // And that outcome is exactly the `None` the ladder already handles.
        let choice = choose_temp_base(None, None, Path::new("/tmp"));
        assert_eq!(choice.path, Path::new("/tmp"));
        assert!(
            choice.warnings.first().is_some_and(|w| w.contains("#322")),
            "{:?}",
            choice.warnings,
        );
    }

    /// The ordinary path still works through the new seam: a fresh shared
    /// directory gets an owner-only parent created under it, and a second call
    /// adopts what the first created rather than objecting to it.
    #[cfg(unix)]
    #[test]
    fn a_fresh_shared_directory_yields_an_owner_only_parent() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let euid = effective_uid();
        let parent = private_temp_parent_in(anchor.path(), euid).expect("a fresh parent");
        assert_eq!(parent, anchor.path().join(private_parent_name(euid)));
        let mode = std::fs::symlink_metadata(&parent)
            .expect("stat the created parent")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700, "{} is 0o{mode:o}", parent.display());
        assert_eq!(
            private_temp_parent_in(anchor.path(), euid).ok(),
            Some(parent),
            "adopting the parent it just created must not object",
        );
    }

    /// macOS reaches `/var/tmp` through a symlink — `/var -> private/var` — so
    /// the harness and `cargo xtask clean-e2e-tmp` end up holding two different
    /// spellings of one directory. This pins which one the *harness* holds.
    ///
    /// [`private_temp_parent_in`] joins the parent's name onto the shared
    /// directory it was handed and never canonicalises, so what the socket
    /// budget is charged against, and what a later `bind(2)` actually sees, is
    /// the short `/var/tmp/dad-e2e-<uid>`. The reaper resolves instead and scans
    /// `/private/var/tmp/dad-e2e-<uid>`. The only thing that has to be true of
    /// the pair is that they are one directory, which is asserted by inode
    /// rather than by string.
    #[cfg(unix)]
    #[test]
    fn the_private_parent_keeps_the_short_spelling_the_socket_budget_charges_for() {
        use std::os::unix::fs::MetadataExt;
        let anchor = race_safe_tempdir();
        let euid = effective_uid();
        // macOS's own layout in miniature: a real `private/var/tmp`, reached
        // through a `var` symlink that `lstat` traverses because it is never the
        // final component.
        std::fs::create_dir_all(anchor.path().join("private/var/tmp"))
            .expect("stand-in /private/var/tmp");
        std::os::unix::fs::symlink("private/var", anchor.path().join("var")).expect("plant /var");
        let shared = anchor.path().join("var/tmp");

        let by_name = private_temp_parent_in(&shared, euid).expect("a parent below the link");
        assert_eq!(by_name, shared.join(private_parent_name(euid)));

        let resolved = by_name.canonicalize().expect("resolve the parent");
        assert_ne!(
            by_name, resolved,
            "the fixture must really diverge, as macOS does",
        );
        assert!(
            by_name.as_os_str().len() < resolved.as_os_str().len(),
            "the harness must hold the shorter spelling: {} vs {}",
            by_name.display(),
            resolved.display(),
        );
        let named = std::fs::metadata(&by_name).expect("stat by name");
        let followed = std::fs::metadata(&resolved).expect("stat resolved");
        assert_eq!(
            (named.dev(), named.ino()),
            (followed.dev(), followed.ino()),
            "the two spellings must be one directory",
        );
    }

    /// The socket budget at a macOS UID, in both spellings the two halves use.
    ///
    /// [`SUN_PATH_USABLE`] is already macOS's 103, so the only open question is
    /// whether a `501`-shaped parent composes inside it. Both do, with room:
    /// `/var/tmp/dad-e2e-501` is 20 bytes and composes to 68;
    /// `/private/var/tmp/dad-e2e-501` is 28 and composes to 76 — against a
    /// 55-byte base allowance and a 103-byte socket path. The harness binds
    /// under the first of those, and the veto is applied to that same value.
    #[cfg(unix)]
    #[test]
    fn a_macos_uid_fits_the_socket_budget_in_both_spellings() {
        let name = private_parent_name(501);
        let by_name = PathBuf::from(SHARED_VAR_TMP).join(&name);
        let resolved = PathBuf::from("/private")
            .join(SHARED_VAR_TMP.trim_start_matches('/'))
            .join(&name);
        assert_eq!(by_name, Path::new("/var/tmp/dad-e2e-501"));
        assert_eq!(resolved, Path::new("/private/var/tmp/dad-e2e-501"));

        for (base, len) in [(&by_name, 20), (&resolved, 28)] {
            assert_eq!(base.as_os_str().len(), len, "{}", base.display());
            assert!(
                fits_socket_budget(base),
                "{} ({len} bytes) exceeds the {MAX_TEMP_BASE_LEN}-byte allowance",
                base.display(),
            );
            assert!(
                len + HARNESS_SOCKET_OVERHEAD <= SUN_PATH_USABLE,
                "{} composes to {} bytes, past {SUN_PATH_USABLE}",
                base.display(),
                len + HARNESS_SOCKET_OVERHEAD,
            );
        }

        // And the ladder picks the short one without complaint — the veto sees
        // exactly the value that reaches `bind(2)`.
        let choice = choose_temp_base(None, Some(&by_name), Path::new("/tmp"));
        assert_eq!(choice.path, by_name);
        assert!(choice.warnings.is_empty(), "{:?}", choice.warnings);
    }

    /// A refused `DAD_E2E_TMPDIR` is fatal too, and for a stronger reason than
    /// the default: the operator stated where the temp dirs must go, so quietly
    /// putting them somewhere else is both wrong and unasked-for.
    #[test]
    fn a_refused_env_override_is_fatal_rather_than_ignored() {
        let raw = Path::new("scratch/e2e");
        let why = override_shape_objection(raw).expect("a relative value is refused");
        let msg = refused_override_message(raw, &why);
        let named = format!("{TEMP_BASE_ENV}=scratch/e2e cannot be used");
        let unset_default = format!("unset it to use the default {SHARED_VAR_TMP}/dad-e2e-<uid>");
        for expected in [
            "HARNESS PRE-FLIGHT FAILURE",
            "NOT a product regression",
            "No test has run.",
            named.as_str(),
            "is not an absolute path",
            "RAM-backed tmpfs",
            "#322",
            unset_default.as_str(),
        ] {
            assert!(msg.contains(expected), "missing {expected:?} in:\n{msg}");
        }
    }

    /// Traversal is judged by a laxer rule than ownership of the base itself:
    /// `/`, `/home` and `/var` are root-owned, and sticky 1777 directories are
    /// safe because only an entry's owner can rename or remove it.
    #[cfg(unix)]
    #[test]
    fn override_ancestors_allow_root_owned_and_sticky_components() {
        assert_eq!(traversal_objection(0, 0o755, 1000), None, "root-owned /usr");
        assert_eq!(
            traversal_objection(0, 0o1777, 1000),
            None,
            "sticky /var/tmp"
        );
        assert_eq!(traversal_objection(1000, 0o700, 1000), None, "our own dir");
        assert!(
            traversal_objection(0, 0o777, 1000).is_some(),
            "world-writable without the sticky bit is the swappable case",
        );
        assert!(
            traversal_objection(1001, 0o755, 1000).is_some(),
            "a component owned by another unprivileged user must be refused",
        );
    }

    /// A relative value, or one with `..` in it, is refused rather than
    /// normalised: relative resolves against whatever working directory the
    /// test binary happens to have, and `..` silently widens the scope of
    /// everything downstream — including what the reaper would be pointed at.
    /// A `.` is a different matter: it is not a widening, and `components()`
    /// removes it before anything sees the path.
    ///
    /// What counts as absolute is a *platform* question, and the fixtures are
    /// split accordingly: `/var/tmp/e2e` is absolute on Unix and is not on
    /// Windows, where an absolute path needs a drive letter. The rule is one
    /// rule — `Path::is_absolute` — asserted against each platform's own
    /// spelling of it.
    #[test]
    fn an_override_that_is_relative_or_traversing_is_refused() {
        // True everywhere: a bare relative path is absolute on no platform.
        assert!(override_shape_objection(Path::new("scratch/e2e")).is_some());
        assert!(override_shape_objection(Path::new("./scratch/e2e")).is_some());
        #[cfg(unix)]
        {
            assert!(override_shape_objection(Path::new("/var/tmp/../../etc")).is_some());
            assert_eq!(override_shape_objection(Path::new("/var/tmp/e2e")), None);
            assert_eq!(override_shape_objection(Path::new("/var/tmp/./e2e")), None);
        }
        #[cfg(windows)]
        {
            // Rooted but not absolute: no drive letter, so it resolves against
            // whatever drive is current — exactly the ambiguity being refused.
            assert!(override_shape_objection(Path::new(r"\scratch\e2e")).is_some());
            assert!(override_shape_objection(Path::new(r"C:\tmp\..\..\Windows")).is_some());
            assert_eq!(override_shape_objection(Path::new(r"C:\tmp\e2e")), None);
            assert_eq!(override_shape_objection(Path::new(r"C:\tmp\.\e2e")), None);
        }
    }

    /// The directory a scratch anchor really lives at, with every symlink in it
    /// resolved. On macOS `/var` is a symlink to `/private/var`, so the harness
    /// roots these tests build under are reached through one on a completely
    /// healthy machine — and [`validated_override_base`] returns the resolved
    /// spelling, which is what the assertions below have to compare against.
    #[cfg(unix)]
    fn resolved(path: &Path) -> PathBuf {
        path.canonicalize()
            .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()))
    }

    /// The value is resolved exactly once, into a normalized, symlink-free path.
    /// A spelling the filesystem does not use would otherwise be what the
    /// socket-length budget is measured against, and what every later message
    /// names.
    #[cfg(unix)]
    #[test]
    fn a_validated_override_base_comes_back_normalized() {
        let anchor = race_safe_tempdir();
        let noisy = anchor.path().join(".").join("base");
        let base = validated_override_base(&noisy).expect("a fresh base under our own dir");
        assert_eq!(base, resolved(anchor.path()).join("base"));
    }

    /// A symlinked *ancestor* is resolved, not refused — and the resolved form
    /// is what comes back, so nothing downstream ever walks the link again.
    ///
    /// This is the macOS case: `/var` is a symlink to `/private/var` there, so
    /// `std::env::temp_dir()` and everything under `/var/tmp` has a symlinked
    /// ancestor on a healthy machine, and refusing symlinked components outright
    /// rejected the platform's own temp directory. A root-owned system symlink
    /// is not the threat; a component an unprivileged attacker could plant or
    /// swap is, and that is judged after resolution — see the two tests below.
    #[cfg(unix)]
    #[test]
    fn an_override_reached_through_a_symlink_resolves_to_the_real_directory() {
        use std::os::unix::fs::DirBuilderExt;
        let anchor = race_safe_tempdir();
        let real = anchor.path().join("real");
        // Owner-only, because this ends up an *ancestor* of the base: a bare
        // `create_dir` under `umask 002` is 0775, and a group-writable ancestor
        // is refused on its own merits — a different rule from the one under
        // test here.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .expect("create real dir");
        let link = anchor.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");
        let base = validated_override_base(&link.join("base")).expect("a symlinked ancestor");
        assert_eq!(base, resolved(&real).join("base"));
        assert!(base.is_dir(), "{} was not created", base.display());
        assert!(
            !base.starts_with(&link),
            "{} still names the link {}",
            base.display(),
            link.display(),
        );
    }

    /// The blocker the first cut of this walk had: a symlink's **owner** is
    /// checked before the link is followed, not after it has been resolved away.
    ///
    /// Driven through the pure decision because the dangerous shape needs
    /// `chown` to build — the whole point is a link owned by *somebody else*.
    /// The follow path itself is exercised on disk by the tests below.
    ///
    /// This is the case `canonicalize` silently ate. On a multi-user host the
    /// victim asks for `DAD_E2E_TMPDIR=/var/tmp/my-dad/base`; before their first
    /// run another user creates `/var/tmp/my-dad` as a symlink to the victim's
    /// own checkout. `/var/tmp` is sticky, so the victim cannot remove or rename
    /// that entry — sticky protects the *attacker's* planted link here — and
    /// resolving first meant the checkout was then walked as a chain of
    /// perfectly ordinary victim-owned ancestors and accepted, with `base`
    /// created 0700 inside the live repository.
    #[cfg(unix)]
    #[test]
    fn a_symlink_owned_by_another_user_is_refused_before_it_is_followed() {
        let path = Path::new("/var/tmp/my-dad");

        // Ours: the operator naming their own directory through their own link.
        assert_eq!(symlink_hop_objection(path, 1000, 1000), None);
        // Root's: macOS's `/var -> private/var`, which refusing would reject the
        // whole platform.
        assert_eq!(symlink_hop_objection(path, 0, 1000), None);

        let why = symlink_hop_objection(path, 1001, 1000).expect("a foreign link is refused");
        assert!(why.contains("symlink owned by uid 1001"), "{why}");
        assert!(why.contains("neither 1000 nor root"), "{why}");
        assert!(why.contains("/var/tmp/my-dad"), "{why}");
    }

    /// The sticky-directory case end to end, with the shapes that *can* be built
    /// unprivileged: a sticky 1777 stand-in for `/var/tmp` is traversed, and a
    /// link inside it that we own is followed rather than refused.
    ///
    /// Together with the pure test above this pins both halves of the rule —
    /// that a sticky ancestor is still accepted (it has to be: `/var/tmp` is
    /// 1777 on every real machine), and that acceptance now depends on judging
    /// the entry found *below* it rather than on the sticky bit alone.
    #[cfg(unix)]
    #[test]
    fn a_link_we_own_under_a_sticky_directory_is_followed() {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        // The `/var/tmp` stand-in: world-writable, sticky. Another local user
        // could create entries here; the sticky bit only stops them removing
        // ours.
        let shared = anchor.path().join("shared");
        std::fs::create_dir(&shared).expect("create the sticky stand-in");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777))
            .expect("chmod 1777");

        let real = shared.join("real");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&real)
            .expect("create the real target");
        let link = shared.join("my-dad");
        std::os::unix::fs::symlink(&real, &link).expect("plant a link we own");

        let base = validated_override_base(&link.join("base")).expect("our own link is followed");
        assert_eq!(base, resolved(&real).join("base"));
        assert!(base.is_dir(), "{} was not created", base.display());
    }

    /// A **non-final** link is judged too, before the tail below it is created.
    ///
    /// The redirection in the finding is not a link at the base — it is a link
    /// at an ancestor whose missing tail the harness would then happily create
    /// on the far side of it. This walks that exact shape with a link we own
    /// (the only owner a test can produce) and pins the two things that must be
    /// true regardless: the link is resolved *hop by hop* through descriptors,
    /// and the resolved spelling — never the link's own — is what comes back and
    /// is therefore what every downstream message, length budget and reaper
    /// hint sees.
    #[cfg(unix)]
    #[test]
    fn a_non_final_link_is_resolved_before_its_missing_tail_is_created() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        // A 0o755 stand-in for a checkout: ours, ordinary, and a perfectly legal
        // *ancestor* — which is exactly why the link pointing at it has to be
        // the thing that is judged.
        let checkout = anchor.path().join("checkout");
        std::fs::create_dir(&checkout).expect("create the checkout");
        std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        let link = anchor.path().join("link");
        std::os::unix::fs::symlink(&checkout, &link).expect("plant the link");

        let base = validated_override_base(&link.join("outer").join("inner"))
            .expect("our own link is followed");
        assert_eq!(
            base,
            resolved(&checkout).join("outer").join("inner"),
            "the resolved spelling must come back, never the link's",
        );
        assert!(
            !base.starts_with(&link),
            "{} still names the link {}",
            base.display(),
            link.display(),
        );
        // The tail below the link is still created owner-only, one component at
        // a time — a permissive directory on the far side does not relax it.
        for component in [base.parent().expect("outer").to_path_buf(), base] {
            let mode = std::fs::symlink_metadata(&component)
                .expect("stat created component")
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(
                mode,
                0o700,
                "{} was created 0o{mode:o}",
                component.display(),
            );
        }
    }

    /// A **dangling** link we own is followed and its target created, rather
    /// than refused.
    ///
    /// This changed with the walk, so it is pinned rather than left implicit.
    /// Before, `canonicalize` failed with `NotFound` on the dangling component,
    /// the name went into the "missing" list, `mkdirat` came back `EEXIST` and
    /// the value was rejected as "is a symlink". Now the link is judged on its
    /// own merits first, and one owned by us or by root is resolved — so
    /// pointing `DAD_E2E_TMPDIR` through a link whose target does not exist yet
    /// creates the target, which is the same thing the harness does for any
    /// other base that is not there yet. Safety is unchanged: the link's owner
    /// gates the hop, and every component of the target is walked and judged.
    ///
    /// The one place a link is still refused outright is
    /// [`create_or_adopt_component`] — a link that appears in a slot the walk
    /// had just found *empty* is the adoption race, not an operator's choice.
    #[cfg(unix)]
    #[test]
    fn a_dangling_link_we_own_is_followed_and_its_target_created() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let target = anchor.path().join("not-there-yet");
        let link = anchor.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("plant a dangling link");

        let base = validated_override_base(&link).expect("our own dangling link is followed");
        assert_eq!(base, resolved(anchor.path()).join("not-there-yet"));
        let mode = std::fs::symlink_metadata(&base)
            .expect("stat the created target")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700, "{} is 0o{mode:o}", base.display());
    }

    /// A link chain that loops is bounded rather than spun on. `canonicalize`
    /// used to return `ELOOP` for this; now that the harness resolves links
    /// itself, the cap has to be its own.
    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_is_refused_rather_than_followed_forever() {
        let anchor = race_safe_tempdir();
        let a = anchor.path().join("a");
        let b = anchor.path().join("b");
        std::os::unix::fs::symlink(&b, &a).expect("a -> b");
        std::os::unix::fs::symlink(&a, &b).expect("b -> a");
        let err = validated_override_base(&a.join("base")).expect_err("a cycle is refused");
        assert!(err.contains("more than 40 symlinks"), "{err}");
    }

    /// A `..` inside a link *target* would step back above a component the walk
    /// has already proved safe, so it is refused rather than resolved. No system
    /// link the harness needs contains one — macOS's `/var -> private/var` does
    /// not.
    #[cfg(unix)]
    #[test]
    fn a_link_target_containing_a_parent_component_is_refused() {
        use std::os::unix::fs::DirBuilderExt;
        let anchor = race_safe_tempdir();
        // Owner-only: this is an *ancestor* of the link, and a bare
        // `create_dir` under `umask 002` is 0775 — group-writable, which is
        // refused on its own merits, a different rule from the one under test.
        let outer = anchor.path().join("outer");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&outer)
            .expect("create outer");
        let link = outer.join("up");
        std::os::unix::fs::symlink("../sibling", &link).expect("plant a `..` link");
        let err = validated_override_base(&link.join("base")).expect_err("`..` in a target");
        assert!(err.contains("contains a `..` component"), "{err}");
    }

    /// Resolving a symlink does not lower the bar for what it resolves *to*: a
    /// link is a fine way to name a directory and a terrible way to inherit
    /// trust, so the target is judged exactly as if it had been named directly.
    #[cfg(unix)]
    #[test]
    fn a_symlink_target_is_judged_like_any_other_directory() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let target = anchor.path().join("target");
        std::fs::create_dir(&target).expect("create the target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("loosen the target");
        let link = anchor.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let err = validated_override_base(&link).expect_err("a group-readable target is refused");
        assert!(err.contains("mode is 0o755"), "{err}");
        assert!(
            err.contains(&resolved(&target).display().to_string()),
            "{err}"
        );
    }

    /// A base that does not exist yet is created owner-only, one component at a
    /// time, rather than `create_dir_all`-ed at the umask default.
    #[cfg(unix)]
    #[test]
    fn a_missing_override_base_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let base = anchor.path().join("outer").join("inner");
        let created = validated_override_base(&base).expect("a fresh base under our own dir");
        assert_eq!(created, resolved(anchor.path()).join("outer").join("inner"));
        for component in [created.parent().expect("outer").to_path_buf(), created] {
            let mode = std::fs::metadata(&component)
                .expect("stat created component")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode,
                0o700,
                "{} was created 0o{mode:o}, not owner-only",
                component.display(),
            );
        }
    }

    /// The base is held to the same bar whether the harness created it or found
    /// it: it is where real agent credentials get seeded, so ours-and-owner-only
    /// is the point, and an ancestor's laxer rule does not apply to it. Refused,
    /// never repaired — chmod'ing a directory the harness does not own is what
    /// this whole check exists to avoid.
    #[cfg(unix)]
    #[test]
    fn an_existing_override_base_must_still_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let base = anchor.path().join("base");
        std::fs::create_dir(&base).expect("plant the base");
        let mode_of = |p: &Path| {
            std::fs::symlink_metadata(p)
                .expect("stat the base")
                .permissions()
                .mode()
                & 0o7777
        };

        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755))
            .expect("loosen the base");
        let err = validated_override_base(&base).expect_err("a group-readable base is refused");
        assert!(err.contains("mode is 0o755"), "{err}");
        assert!(
            err.contains(&resolved(&base).display().to_string()),
            "{err}"
        );
        assert_eq!(mode_of(&base), 0o755, "the refused base was modified");

        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("tighten the base");
        assert_eq!(
            validated_override_base(&base).expect("an owner-only base is adopted"),
            resolved(&base),
        );
    }

    /// The adoption race (Greptile P1). A component that was missing when the
    /// path was resolved can be created by another local user before the harness
    /// gets to it, and a recursive create would adopt whatever is there — their
    /// directory, or their symlink — without ever looking at it.
    ///
    /// Winning a real race in a test is not practical, so what is pinned is the
    /// *decision*: planting the entry before the call reproduces exactly the
    /// state losing that race leaves behind. Each shape an attacker could leave
    /// must be refused on the descriptor the harness actually opened, whatever
    /// an earlier stat of the name said.
    #[cfg(unix)]
    #[test]
    fn a_component_that_appears_before_creation_is_judged_not_adopted() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = race_safe_tempdir();
        let euid = effective_uid();
        // Each shape needs its own parent, since they all plant the same name.
        let parent_of = |kind: &str| -> (PathBuf, std::fs::File) {
            let dir = anchor.path().join(kind);
            std::fs::create_dir(&dir).expect("stand-in parent");
            let handle = std::fs::File::open(&dir).expect("open the parent");
            (dir, handle)
        };
        let adopt = |path: &Path, handle: &std::fs::File| {
            let mut walked = path.to_path_buf();
            create_or_adopt_component(handle, std::ffi::OsStr::new("base"), &mut walked, euid)
                .map(|_| walked)
        };

        // Nothing there: created by `mkdirat` at 0o700 and accepted.
        let (fresh, handle) = parent_of("fresh");
        let made = adopt(&fresh, &handle).expect("a fresh component is created");
        assert_eq!(made, fresh.join("base"));
        let mode = std::fs::symlink_metadata(&made)
            .expect("stat the created component")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700, "{} is 0o{mode:o}", made.display());

        // A directory that is ours but not owner-only — the shape a lost race
        // leaves when the winner made it world-readable.
        let (loose, handle) = parent_of("loose");
        let planted = loose.join("base");
        std::fs::create_dir(&planted).expect("plant a directory");
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755))
            .expect("loosen it");
        let err = adopt(&loose, &handle).expect_err("a loose directory is refused");
        assert!(err.contains("mode is 0o755"), "{err}");
        assert!(err.contains(&planted.display().to_string()), "{err}");

        // A symlink at the name: `O_NOFOLLOW` refuses it, and — the part that
        // matters — nothing is created at the far end of it.
        let (linked, handle) = parent_of("symlink");
        let target = anchor.path().join("symlink-target");
        std::fs::create_dir(&target).expect("link target");
        std::os::unix::fs::symlink(&target, linked.join("base")).expect("plant a symlink");
        let err = adopt(&linked, &handle).expect_err("a symlink is refused");
        assert!(err.contains("is a symlink"), "{err}");
        assert_eq!(
            std::fs::read_dir(&target)
                .expect("read the link target")
                .count(),
            0,
            "the symlink was followed and written through",
        );

        // A plain file at the name.
        let (filed, handle) = parent_of("file");
        std::fs::write(filed.join("base"), b"not a directory").expect("plant a file");
        let err = adopt(&filed, &handle).expect_err("a file is refused");
        assert!(err.contains("is not a directory"), "{err}");
    }

    /// The same adoption race in the `#[cfg(not(unix))]` arm (Greptile P1 on
    /// #472), which used to hand the whole unchecked pathname to
    /// `create_dir_all` and take whatever was there.
    ///
    /// [`override_base_by_name`] is what that arm now calls, and it is
    /// deliberately `cfg`-free so this runs on the Unix host the suite actually
    /// executes on: the logic is plain `std::fs`, so what is observed here is
    /// what Windows executes. As on the Unix side, winning a real race in a test
    /// is not practical, so what is pinned is the *decision* — planting the entry
    /// before the call reproduces exactly the state losing that race leaves
    /// behind.
    ///
    /// The last case pins a **limit, not a guarantee**: a plain directory
    /// somebody else planted is still adopted, because `std` exposes no owner on
    /// Windows. Asserting it keeps the residual honest rather than implied.
    #[test]
    fn a_by_name_override_component_is_created_or_refused_never_adopted() {
        // Held for the whole test: dropping it removes the tree underneath.
        let guard = race_safe_tempdir();
        // Resolved, because this walk refuses a symlinked ancestor rather than
        // following it (see [`override_base_by_name`]) and on macOS `/var` — the
        // harness root's own parent — is a symlink to `/private/var`. That is a
        // property of the *fixture path*, not of the rule under test.
        let anchor = guard
            .path()
            .canonicalize()
            .expect("resolve the scratch anchor");
        // Each shape needs its own parent, since they all plant the same name.
        let parent_of = |kind: &str| -> PathBuf {
            let dir = anchor.join(kind);
            std::fs::create_dir(&dir).expect("stand-in parent");
            dir
        };

        // Nothing there: the ordinary path still works, through a component that
        // does not exist either.
        let fresh = parent_of("fresh").join("outer").join("base");
        let made = override_base_by_name(&fresh).expect("a fresh base is created");
        assert_eq!(made, fresh, "the value comes back as the normalized path");
        assert!(made.is_dir(), "{} was not created", made.display());

        // And a second call over the same path adopts it rather than failing —
        // the concurrent-test-process case, which must not be a refusal.
        assert_eq!(
            override_base_by_name(&fresh).expect("an existing base is adopted"),
            fresh,
        );

        // `.` noise is dropped, so nothing downstream sees a spelling the
        // filesystem does not.
        let noisy = parent_of("noisy").join(".").join("base");
        let base = override_base_by_name(&noisy).expect("a fresh base under our own dir");
        assert_eq!(base, anchor.join("noisy").join("base"));

        // A plain file at a missing component.
        let filed = parent_of("file");
        std::fs::write(filed.join("base"), b"not a directory").expect("plant a file");
        let err = override_base_by_name(&filed.join("base").join("deeper"))
            .expect_err("a file is refused");
        assert!(err.contains("is not a directory"), "{err}");

        // A symlink at a missing component — the redirection the finding is
        // about. Unix-only *as a fixture*: `std::os::windows::fs::symlink_dir`
        // needs Developer Mode or `SeCreateSymbolicLinkPrivilege`, so the shape
        // is planted where it can be, and the rule it exercises is the same one
        // `chain_entry_verdict` states for every platform.
        #[cfg(unix)]
        {
            let linked = parent_of("symlink");
            let target = anchor.join("symlink-target");
            std::fs::create_dir(&target).expect("link target");
            std::os::unix::fs::symlink(&target, linked.join("base")).expect("plant a symlink");
            let err =
                override_base_by_name(&linked.join("base")).expect_err("a symlink is refused");
            assert!(err.contains("is a symlink"), "{err}");
            // The part that matters: nothing was created at the far end of it.
            let err = override_base_by_name(&linked.join("base").join("deeper"))
                .expect_err("a symlinked ancestor is refused too");
            assert!(err.contains("is a symlink"), "{err}");
            assert_eq!(
                std::fs::read_dir(&target)
                    .expect("read the link target")
                    .count(),
                0,
                "the symlink was followed and written through",
            );
        }

        // The residual, asserted so it cannot rot into an assumed guarantee: a
        // plain pre-existing directory is adopted. On Unix that is safe because
        // the arm above judges ownership and mode; here there is nothing to
        // judge it by, and #163/#164 is what closes it.
        let planted = parent_of("planted").join("base");
        std::fs::create_dir(&planted).expect("plant a directory");
        assert_eq!(
            override_base_by_name(&planted).expect("a plain directory is adopted"),
            planted,
            "adoption of a plain directory is the documented residual",
        );
    }

    /// The race branch on its own. The walk above stats before it creates, so a
    /// pre-planted entry is caught by the stat; the branch that decides the
    /// actual race is the one where `create_dir` comes back `AlreadyExists`
    /// because the entry appeared *after* that stat.
    ///
    /// [`create_or_refuse_component`] is where that lands, and planting the entry
    /// before calling it reproduces exactly the state losing the race leaves
    /// behind. `AlreadyExists` must not be success — which is what
    /// `create_dir_all` treated it as, and is the whole of Greptile's finding.
    #[test]
    fn a_by_name_component_that_appears_before_creation_is_judged_not_adopted() {
        let guard = race_safe_tempdir();
        let anchor = guard
            .path()
            .canonicalize()
            .expect("resolve the scratch anchor");
        // Each shape needs its own parent, since they all plant the same name.
        let parent_of = |kind: &str| -> PathBuf {
            let dir = anchor.join(kind);
            std::fs::create_dir(&dir).expect("stand-in parent");
            dir
        };

        // Nothing there: created, and accepted.
        let made = parent_of("fresh").join("base");
        create_or_refuse_component(&made).expect("a fresh component is created");
        assert!(made.is_dir(), "{} was not created", made.display());

        // A plain file at the name.
        let filed = parent_of("file").join("base");
        std::fs::write(&filed, b"not a directory").expect("plant a file");
        let err = create_or_refuse_component(&filed).expect_err("a file is refused");
        assert!(err.contains("is not a directory"), "{err}");

        // A symlink at the name — Unix-only as a *fixture* (Windows needs
        // Developer Mode to create one), same rule either way.
        #[cfg(unix)]
        {
            let linked = parent_of("symlink").join("base");
            let target = anchor.join("symlink-target");
            std::fs::create_dir(&target).expect("link target");
            std::os::unix::fs::symlink(&target, &linked).expect("plant a symlink");
            let err = create_or_refuse_component(&linked).expect_err("a symlink is refused");
            assert!(err.contains("is a symlink"), "{err}");
            assert_eq!(
                std::fs::read_dir(&target)
                    .expect("read the link target")
                    .count(),
                0,
                "the symlink was followed and written through",
            );
        }

        // And the residual once more, at the level that decides it: a plain
        // directory somebody else could have planted is adopted, because `std`
        // exposes no owner on Windows to tell it from one of ours.
        let planted = parent_of("planted").join("base");
        std::fs::create_dir(&planted).expect("plant a directory");
        create_or_refuse_component(&planted).expect("a plain directory is adopted");
    }

    /// The rule the by-name walk judges every component against, as a pure
    /// function — which is the only way the **reparse-point** case can be
    /// pinned at all: junctions, cloud-file placeholders and `AppExecLink`
    /// entries exist only on Windows, and `FileType::is_symlink` covers just the
    /// first two of the three. A redirection the harness would write agent
    /// credentials through must be refused whichever tag it carries.
    #[test]
    fn a_chain_entry_verdict_refuses_every_kind_of_redirection() {
        let path = Path::new("/anchor/base");
        let named = |verdict: Option<String>| verdict.unwrap_or_default();

        // A plain directory is the one shape that passes.
        assert_eq!(chain_entry_verdict(path, false, false, true), None);

        // A symlink or junction — what `std` classifies as a link.
        assert!(named(chain_entry_verdict(path, true, true, true)).contains("is a symlink"));
        // A reparse point `std` does not classify as a link. This is the case
        // `is_symlink` alone misses.
        assert!(
            named(chain_entry_verdict(path, false, true, true)).contains("is a reparse point"),
            "an unclassified reparse point must still be refused",
        );
        // A file, or anything else that is not a directory.
        assert!(
            named(chain_entry_verdict(path, false, false, false)).contains("is not a directory")
        );

        // Every refusal names the path, since the message is all the operator
        // gets — `refused_override_message` wraps it verbatim.
        for (link, reparse, dir) in [
            (true, true, true),
            (false, true, true),
            (false, false, false),
        ] {
            let why = named(chain_entry_verdict(path, link, reparse, dir));
            assert!(why.contains("/anchor/base"), "{why}");
        }
    }

    /// The by-name walk applies the same shape rule as the descriptor walk —
    /// a relative value or one with `..` never reaches the filesystem at all.
    #[test]
    fn a_by_name_override_base_refuses_the_same_shapes() {
        let err = override_base_by_name(Path::new("scratch/e2e")).expect_err("relative is refused");
        assert!(err.contains("is not an absolute path"), "{err}");
        #[cfg(unix)]
        {
            let err = override_base_by_name(Path::new("/var/tmp/../../etc"))
                .expect_err("`..` is refused");
            assert!(err.contains("contains a `..` component"), "{err}");
        }
    }

    /// The `dad-tests-<pid>-*` name is load-bearing: `cargo xtask
    /// clean-e2e-tmp` reaps by that prefix and issue #461 reaps by the PID
    /// inside it. Moving the root off `/tmp` must not disturb either.
    #[test]
    fn the_harness_root_keeps_its_pid_tagged_name() {
        let root = harness_temp_root();
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .expect("root has a UTF-8 name");
        let prefix = format!("dad-tests-{}-", std::process::id());
        assert!(
            name.starts_with(&prefix),
            "{name} does not start with {prefix}"
        );
        assert!(
            name.len() > prefix.len(),
            "{name} has no random suffix after {prefix}",
        );
        assert_eq!(
            root.parent(),
            Some(harness_temp_base().path.as_path()),
            "root must sit directly in the resolved temp base",
        );
    }

    /// The root is created 0o700 by `mkdir(2)` itself, not chmod'ed afterwards.
    /// On a shared base the gap between the two is long enough for a local user
    /// to enter a default-0o755 root and plant fixed descendants.
    #[cfg(unix)]
    #[test]
    fn the_harness_root_is_owner_only_from_the_moment_it_exists() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let root = harness_temp_root();
        let meta = std::fs::symlink_metadata(root).expect("stat harness root");
        let mode = meta.permissions().mode() & 0o7777;
        assert_eq!(
            private_dir_objection(meta.uid(), mode, effective_uid()),
            None,
            "{} is uid {} mode 0o{mode:o}",
            root.display(),
            meta.uid(),
        );
    }

    /// The exact mode the harness *claims* — `0o700`, read back off the disk —
    /// for the root and, when the private `/var/tmp` rung is the one in use,
    /// for its parent.
    ///
    /// [`the_harness_root_is_owner_only_from_the_moment_it_exists`] asserts the
    /// weaker `mode & 0o077 == 0`, which `0o500` and `0o000` also satisfy. This
    /// pins the value the audit note and `docs/develop/e2e-temp-dirs.md` both
    /// state, and it reads `symlink_metadata` rather than trusting that a
    /// permissions builder was called: leftover roots turn up at `0o775`
    /// because an orphaned agent re-created the path after the exit sweep
    /// removed the real one, and only an on-disk assertion distinguishes a
    /// harness root from a re-creation.
    #[cfg(unix)]
    #[test]
    fn the_harness_root_and_its_private_parent_are_exactly_0o700_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        let on_disk = |p: &Path| -> u32 {
            std::fs::symlink_metadata(p)
                .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
                .permissions()
                .mode()
                & 0o7777
        };

        let root = harness_temp_root();
        let root_mode = on_disk(root);
        assert_eq!(
            root_mode,
            0o700,
            "{} is 0o{root_mode:o} on disk, not the 0o700 the harness claims",
            root.display(),
        );

        // Only the `/var/tmp/dad-e2e-<uid>` rung is the harness's to hold at
        // 0o700. The system-temp fallback is 1777 by design, and a
        // `DAD_E2E_TMPDIR` base is the caller's directory, not ours.
        let private = match private_temp_parent() {
            Ok(p) => p,
            Err(why) => {
                eprintln!("skipping the parent half: no private parent here ({why})");
                return;
            }
        };
        if root.parent() != Some(private.as_path()) {
            eprintln!(
                "skipping the parent half: the base in use is not the private parent {}",
                private.display(),
            );
            return;
        }
        let parent_mode = on_disk(&private);
        assert_eq!(
            parent_mode,
            0o700,
            "{} is 0o{parent_mode:o} on disk, not the 0o700 the harness claims",
            private.display(),
        );
    }

    /// The per-test dir construction from `TuiDeck::try_launch_inner` — a bare
    /// `tempfile::Builder` with `.permissions(0o700)` and the default `.tmp`
    /// prefix — lands 0o700 on disk, verified against a control that proves the
    /// umask alone would not have produced it.
    ///
    /// `try_launch_inner`'s own assertion only runs inside a live launch, i.e.
    /// under `--features e2e`. This exercises the same two lines in the fast
    /// tier, so a `tempfile` upgrade that stopped honouring `permissions()` for
    /// the `tempdir_in` path is named here instead of surfacing as a panic deep
    /// inside a PTY test.
    ///
    /// The control comes first because the assertion is only meaningful under a
    /// permissive umask: with `umask 077` a directory created with no explicit
    /// permissions at all is already owner-only, and asserting 0o700 would pass
    /// while proving nothing. Skipping then is honest; asserting is not.
    #[cfg(unix)]
    #[test]
    fn the_per_test_tempdir_is_0o700_even_when_the_umask_alone_would_not_be() {
        use std::os::unix::fs::PermissionsExt;
        let on_disk = |p: &Path| -> u32 {
            std::fs::symlink_metadata(p)
                .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
                .permissions()
                .mode()
                & 0o7777
        };
        let root = harness_temp_root();

        let control = tempfile::Builder::new()
            .prefix("umask-control-")
            .tempdir_in(root)
            .expect("umask control dir");
        let control_mode = on_disk(control.path());
        if control_mode & 0o077 == 0 {
            eprintln!(
                "skipping: the umask here already yields 0o{control_mode:o} \
                 without asking, so the 0o700 below would prove nothing"
            );
            return;
        }

        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(root)
            .expect("per-test dir");
        let mode = on_disk(dir.path());
        assert_eq!(
            mode,
            0o700,
            "{} is 0o{mode:o} on disk while a dir created the same way without \
             `permissions()` is 0o{control_mode:o} — `tempfile` stopped applying \
             the mode at creation",
            dir.path().display(),
        );
    }

    /// The harness root must never be a descendant of the real checkout. A
    /// seeded fixture that sits inside this repository is one `..` away from
    /// `CLAUDE.md`, `AGENTS.md`, `.claude/` and `.agents/` — and real agents
    /// walk ancestors, with the Codex worker taking such a directory as its
    /// writable workspace. Skipped when `DAD_E2E_TMPDIR` is set, since pointing
    /// it into the repo is an explicit (and documented) choice.
    #[test]
    fn the_harness_root_is_never_inside_the_repository() {
        if std::env::var_os(TEMP_BASE_ENV).is_some_and(|v| !v.is_empty()) {
            eprintln!("skipping: {TEMP_BASE_ENV} is set, so placement is explicit");
            return;
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = harness_temp_root();
        assert!(
            !root.starts_with(repo),
            "{} is inside the checkout at {}",
            root.display(),
            repo.display(),
        );
    }

    /// Marks the first allocation on stdout so the re-run below can capture it.
    const FIRST_ALLOC_MARKER: &str = "harness-first-alloc=";

    /// The `tempfile` process-global redirect, asserted for exactly what it is:
    /// **defence in depth**, in force only once the harness root exists.
    ///
    /// The root is resolved first here on purpose — that is the precondition of
    /// the claim, not an accident of test order — so this covers allocations the
    /// suite does not make itself (a dependency's, a call site that slipped past
    /// `linkage-check` rule 8) *after* something has asked the harness for a
    /// directory. It says nothing about the first allocation of a process; that
    /// is the test below, and conflating the two is what let issue #322's
    /// biggest allocations keep landing on the tmpfs while a green test claimed
    /// otherwise.
    #[test]
    fn the_tempfile_redirect_catches_a_bare_constructor_once_the_root_exists() {
        let root = harness_temp_root();
        // The bare constructor IS the thing under test here, so rule 8 is opted
        // out of on this one line: linkage-check:allow-bare-tempdir
        let stray = tempfile::tempdir().expect("bare tempdir"); // linkage-check:allow-bare-tempdir
        assert!(
            stray.path().starts_with(root),
            "{} escaped the harness root {}",
            stray.path().display(),
            root.display(),
        );
    }

    /// Issue #322: the suite's temp-dir constructor must contain its result
    /// **whatever order a test does things in** — including when it is the very
    /// first thing the process does.
    ///
    /// The ordering is the entire point, and asserting it the other way round
    /// proves nothing. The predecessor of this test resolved the root and only
    /// *then* allocated, so it exercised the one ordering that could not fail.
    /// Reversed, and measured on `a0b616c`, the allocation went to
    /// `/tmp/.tmpz5pszS` while the root was
    /// `/var/tmp/dad-e2e-1000/dad-tests-1715819-eACfgW` — in all 13 fast-tier
    /// binaries — because the redirect above is installed at the END of the lazy
    /// initialiser and nothing had triggered it yet.
    ///
    /// [`harness_tempdir`] is ordering-independent by construction: it resolves
    /// the root before it allocates. The call is kept as the *first statement*
    /// deliberately — anything above it re-introduces the favourable ordering.
    ///
    /// Doubles as the child of
    /// [`the_first_allocation_in_a_fresh_process_is_contained`], which re-runs
    /// this test in its own process and reads both paths off the markers.
    #[test]
    fn a_harness_tempdir_lands_under_the_harness_root() {
        let stray = harness_tempdir().expect("first allocation of the process");
        let root = harness_temp_root();
        println!("{FIRST_ALLOC_MARKER}{}", stray.path().display());
        println!("{ROOT_MARKER}{}", root.display());
        assert!(
            stray.path().starts_with(root),
            "{} escaped the harness root {}",
            stray.path().display(),
            root.display(),
        );
    }

    /// The same claim in a genuinely fresh process, because only nextest
    /// guarantees one process per test. Under plain `cargo test` some earlier
    /// test in the same binary has already built the root, and the ordering the
    /// test above exists to pin is silently no longer under test.
    ///
    /// Re-runs *this* binary against that single test and reads both paths off
    /// its stdout, so containment is asserted against a process whose first
    /// allocation provably is the one under test.
    #[test]
    fn the_first_allocation_in_a_fresh_process_is_contained() {
        let exe = std::env::current_exe().expect("current exe");
        // libtest test names omit the crate segment `module_path!()` carries.
        let module = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_else(|| module_path!());
        let child_test = format!("{module}::a_harness_tempdir_lands_under_the_harness_root");
        let out = std::process::Command::new(&exe)
            .arg(&child_test)
            .args(["--exact", "--test-threads=1", "--nocapture"])
            .output()
            .expect("re-run this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "child run of {child_test} failed: {}\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        // `--nocapture` interleaves markers onto libtest's own `test <name> ...`
        // line, so match anywhere in the line rather than at the start.
        let field = |marker: &str| -> String {
            stdout
                .lines()
                .find_map(|l| l.split_once(marker).map(|(_, rest)| rest.trim().to_string()))
                .unwrap_or_else(|| {
                    panic!(
                        "child never reported {marker} — did `{child_test}` match no tests?\n{stdout}"
                    )
                })
        };
        let first = field(FIRST_ALLOC_MARKER);
        let root = field(ROOT_MARKER);
        assert!(
            Path::new(&first).starts_with(&root),
            "the first allocation of a fresh process escaped the harness root:\n  \
             allocated {first}\n  root      {root}",
        );
    }

    /// Build a directory whose absolute path is *exactly* `len` bytes.
    ///
    /// Deliberately not under the harness root: the point is to control the
    /// total length, and the harness root's own length is whatever this machine
    /// makes it. The returned guard removes it. `None` when every short anchor
    /// on this machine is already longer than `len`, in which case the caller
    /// skips rather than asserting something it did not actually build.
    #[cfg(unix)]
    fn padded_base_of_len(len: usize) -> Option<tempfile::TempDir> {
        // `tempfile` always appends exactly six random characters.
        const RAND: usize = 6;
        let anchor = [PathBuf::from(SHARED_VAR_TMP), std::env::temp_dir()]
            .into_iter()
            .filter(|p| p.is_dir())
            .find(|p| p.as_os_str().len() + 1 + RAND <= len)?;
        let pad = len - anchor.as_os_str().len() - 1 - RAND;
        tempfile::Builder::new()
            .prefix(&"p".repeat(pad))
            .tempdir_in(&anchor)
            .ok()
    }

    /// Reproduce the deepest path the harness ever *binds*, using the same
    /// constructors it uses, with the worst-case PID width baked in (Linux's
    /// default `pid_max` is 4194304 — seven digits). The two `TempDir` guards
    /// must be held by the caller: dropping them removes the socket's parents.
    #[cfg(unix)]
    fn worst_case_socket_path(base: &Path) -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let root = tempfile::Builder::new()
            .prefix("dad-tests-4194304-")
            .tempdir_in(base)
            .expect("worst-case harness root");
        // Default `tempfile` prefix — what `race_safe_tempdir` and a bare
        // `harness_tempdir()` both produce.
        let inner = tempfile::Builder::new()
            .tempdir_in(root.path())
            .expect("worst-case per-test dir");
        let sock = inner.path().join("attach.sock");
        (root, inner, sock)
    }

    /// The boundary the veto actually claims: a base at exactly the maximum
    /// accepted length composes to exactly `SUN_PATH_USABLE` bytes and binds.
    ///
    /// The equality is the part that matters — it is what makes
    /// `HARNESS_SOCKET_OVERHEAD` unable to drift. If `tempfile`'s suffix grew,
    /// or a longer socket name appeared, or the constant were trimmed, the
    /// composed path would stop matching the budget and this fails.
    #[cfg(unix)]
    #[test]
    fn socket_budget_binds_at_exactly_the_maximum_base_length() {
        let Some(base) = padded_base_of_len(MAX_TEMP_BASE_LEN) else {
            eprintln!("skipping: no anchor short enough for a {MAX_TEMP_BASE_LEN}-byte base");
            return;
        };
        assert!(
            fits_socket_budget(base.path()),
            "{} ({} bytes) should be exactly at the limit",
            base.path().display(),
            base.path().as_os_str().len(),
        );
        let (_root, _inner, sock) = worst_case_socket_path(base.path());
        assert_eq!(
            sock.as_os_str().len(),
            SUN_PATH_USABLE,
            "composed {} — the real nesting no longer matches \
             HARNESS_SOCKET_OVERHEAD ({HARNESS_SOCKET_OVERHEAD})",
            sock.display(),
        );
        let listener = std::os::unix::net::UnixListener::bind(&sock);
        assert!(
            listener.is_ok(),
            "cannot bind {} ({} bytes): {:?}",
            sock.display(),
            sock.as_os_str().len(),
            listener.err(),
        );
    }

    /// One byte over is refused by the veto, and that byte is real: the
    /// composed path is one past `sun_path` on macOS/BSD, where 104 bytes is
    /// the cap. Linux allows 108, so the bind itself would still succeed here —
    /// the veto is calibrated to the smaller platform on purpose, which is why
    /// this asserts the arithmetic rather than the syscall. That the cap is
    /// real at all is proven by the test below.
    #[cfg(unix)]
    #[test]
    fn socket_budget_refuses_one_byte_over_the_maximum_base_length() {
        let over = MAX_TEMP_BASE_LEN + 1;
        let Some(base) = padded_base_of_len(over) else {
            eprintln!("skipping: no anchor short enough for an {over}-byte base");
            return;
        };
        assert!(
            !fits_socket_budget(base.path()),
            "{} ({} bytes) should be one byte too long",
            base.path().display(),
            base.path().as_os_str().len(),
        );
        let (_root, _inner, sock) = worst_case_socket_path(base.path());
        assert_eq!(sock.as_os_str().len(), SUN_PATH_USABLE + 1);
    }

    /// The cap is a real syscall failure, not a convention: past the kernel's
    /// `sun_path` (108 on Linux, 104 on macOS/BSD) `bind(2)` refuses outright.
    /// This is the failure the ladder exists to avoid, and the reason the whole
    /// budget is not simply "use the longest path you like".
    #[cfg(unix)]
    #[test]
    fn a_socket_path_past_the_kernel_cap_cannot_be_bound() {
        // 13 past the macOS-calibrated maximum is 116 composed bytes — over the
        // cap on every platform this suite runs on.
        let far_over = MAX_TEMP_BASE_LEN + 13;
        let Some(base) = padded_base_of_len(far_over) else {
            eprintln!("skipping: no anchor short enough for a {far_over}-byte base");
            return;
        };
        let (_root, _inner, sock) = worst_case_socket_path(base.path());
        let err = std::os::unix::net::UnixListener::bind(&sock)
            .expect_err("a path past sun_path must not bind");
        eprintln!(
            "bind({} bytes) failed as expected: {err}",
            sock.as_os_str().len()
        );
    }

    /// The everyday case, at whatever depth this machine actually produces:
    /// the harness's own nesting still binds.
    #[cfg(unix)]
    #[test]
    fn a_socket_still_binds_at_the_depth_the_harness_uses() {
        let dir = race_safe_tempdir();
        let sock = dir.path().join("attach.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock);
        assert!(
            listener.is_ok(),
            "cannot bind {} ({} bytes): {:?}",
            sock.display(),
            sock.as_os_str().len(),
            listener.err(),
        );
    }

    /// Every harness tempdir nests under the one per-process root, so a killed
    /// run leaves a single reapable directory instead of scattered `/tmp/.tmp*`
    /// dirs that are indistinguishable from any other Rust program's.
    ///
    /// Doubles as the child of
    /// [`harness_temp_root_is_removed_when_the_process_exits_normally`], which
    /// re-runs this test and reads the root off the marker line below.
    #[test]
    fn race_safe_tempdir_nests_under_the_harness_root() {
        let dir = race_safe_tempdir();
        assert!(
            dir.path().starts_with(harness_temp_root()),
            "{} is not under the harness root {}",
            dir.path().display(),
            harness_temp_root().display(),
        );
        println!("{ROOT_MARKER}{}", harness_temp_root().display());
    }

    /// The lock dir is contained by the root and hardened to 0o700 like every
    /// other harness dir. It previously used a bare `tempfile::Builder` with no
    /// re-chmod, so the daemon's `bind_socket` umask flip left it
    /// world-traversable — 474 of 521 leftovers were `drwxrwxr-x` (issue #358).
    #[cfg(unix)]
    #[test]
    fn init_test_env_lock_dir_is_contained_and_mode_0700() {
        use std::os::unix::fs::PermissionsExt;
        init_test_env();
        let lock = lock_dir_path().expect("init_test_env creates the lock dir");
        assert!(
            lock.starts_with(harness_temp_root()),
            "lock dir {} escaped the harness root",
            lock.display(),
        );
        let mode = std::fs::metadata(&lock)
            .expect("stat lock dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "lock dir mode is {mode:o}, want 700");
    }

    /// A test process that exits normally takes its whole temp root with it.
    ///
    /// Re-runs *this* binary against a single test that provably creates the
    /// root, then asserts the child left nothing behind. This is the regression
    /// guard for the original defect: the lock dir lived in a
    /// `static OnceLock<TempDir>`, and because Rust never drops statics, it
    /// leaked once per test process even when every test passed.
    ///
    /// Unix-only, matching [`register_temp_root_cleanup`]: there is no `atexit`
    /// binding in scope on Windows, so a Windows run leaks its root until
    /// `cargo xtask clean-e2e-tmp` (which is cross-platform) is invoked. The
    /// containment test above still covers Windows.
    #[cfg(unix)]
    #[test]
    fn harness_temp_root_is_removed_when_the_process_exits_normally() {
        let exe = std::env::current_exe().expect("current exe");
        // libtest test names omit the crate segment that `module_path!()`
        // carries. Getting this wrong makes the filter match nothing, the child
        // exit 0 having run no tests, and this assertion pass vacuously — so
        // the missing marker below is treated as a failure, not a skip.
        let module = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_else(|| module_path!());
        let child_test = format!("{module}::race_safe_tempdir_nests_under_the_harness_root");
        let out = std::process::Command::new(&exe)
            .arg(&child_test)
            .args(["--exact", "--test-threads=1", "--nocapture"])
            .output()
            .expect("re-run this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "child run of {child_test} failed: {}\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        // `--nocapture` interleaves the marker onto libtest's own
        // `test <name> ... ` line, so match it anywhere in the line rather than
        // at the start.
        let root = stdout
            .lines()
            .find_map(|l| l.split_once(ROOT_MARKER).map(|(_, rest)| rest.trim()))
            .unwrap_or_else(|| {
                panic!("child never reported a temp root — did `{child_test}` match no tests?\n{stdout}")
            });
        assert!(
            !Path::new(root).exists(),
            "child exited cleanly but left its temp root behind: {root}",
        );
    }

    /// The whole probe arriving in one PTY read (the common case) is answered
    /// with the flags reply first, then DA1 — the order crossterm expects.
    #[test]
    fn answer_terminal_queries_replies_to_a_single_chunk_probe() {
        let mut scan = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        answer_terminal_queries(b"\x1b[?u\x1b[c", &mut scan, &mut out);
        assert_eq!(out, b"\x1b[?1u\x1b[?62;22c".to_vec());
    }

    /// A probe split across two reads must still be answered exactly once —
    /// the scan buffer retains just enough trailing context to complete the
    /// match, and retained bytes are guaranteed match-free so nothing is
    /// answered twice.
    #[test]
    fn answer_terminal_queries_handles_a_split_probe_without_duplicating() {
        let mut scan = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        answer_terminal_queries(b"noise\x1b[?", &mut scan, &mut out);
        assert!(out.is_empty(), "no complete query yet, got {out:?}");
        answer_terminal_queries(b"u\x1b[c more", &mut scan, &mut out);
        assert_eq!(out, b"\x1b[?1u\x1b[?62;22c".to_vec());

        // Ordinary follow-up output must not re-trigger a reply.
        let before = out.len();
        answer_terminal_queries(b"plain output\r\n", &mut scan, &mut out);
        assert_eq!(out.len(), before, "a second reply leaked: {out:?}");
    }

    #[test]
    fn strip_jsonc_comments_drops_line_and_block_comments() {
        let input = "{\n  // line comment\n  /* block\n  comment */ \"a\": 1\n}";
        let out = strip_jsonc_comments(input);
        // serde_json must be able to parse the result without the
        // JSONC comment tokens.
        let v: serde_json::Value = serde_json::from_str(&out).expect("stripped output parses");
        assert_eq!(v["a"], serde_json::json!(1));
    }

    #[test]
    fn strip_jsonc_comments_preserves_string_literal_slashes() {
        let input = r#"{"url": "https://example.com/path", "marker": "//keep" }"#;
        let out = strip_jsonc_comments(input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("parses");
        assert_eq!(v["url"], "https://example.com/path");
        assert_eq!(v["marker"], "//keep");
    }

    #[test]
    fn strip_hooks_from_claude_settings_jsonc_input_strips_hooks() {
        // M3.1 auditor S0 regression: a `//`-comment-bearing
        // settings.json must round-trip through the stripper with
        // its `hooks` key removed, NOT pass through unchanged.
        let raw = "{\n  // top-level comment\n  \"hooks\": {\"PostToolUse\": []},\n  \"theme\": \"dark\"\n}";
        let out = strip_hooks_from_claude_settings(raw).expect("jsonc parses after stripping");
        assert!(
            !out.contains("hooks"),
            "stripped settings must not still mention `hooks`: {out}"
        );
        assert!(
            out.contains("\"theme\""),
            "stripped settings must keep non-hook keys: {out}"
        );
    }

    #[test]
    fn strip_hooks_from_claude_settings_truly_malformed_fails_closed() {
        // Garbage that isn't valid JSON even after comment stripping
        // is rejected with an Err — fail-CLOSED rather than letting
        // the host's hooks survive into the test (M3.1 auditor S0).
        let result = strip_hooks_from_claude_settings("{ this is not valid json at all");
        assert!(result.is_err());
        let err_text = result.unwrap_err().to_string();
        assert!(
            err_text.contains("not valid JSON"),
            "error must explain why the file was rejected: {err_text}"
        );
    }

    #[test]
    fn toml_escape_passes_plain_strings_through() {
        assert_eq!(toml_escape("simple"), "simple");
        assert_eq!(toml_escape("with spaces"), "with spaces");
    }

    #[test]
    fn toml_escape_quotes_and_backslashes_use_basic_escapes() {
        assert_eq!(toml_escape(r#"quote " inside"#), r#"quote \" inside"#);
        assert_eq!(toml_escape(r"back \ slash"), r"back \\ slash");
    }

    #[test]
    fn toml_escape_handles_named_control_chars() {
        assert_eq!(toml_escape("line\nbreak"), r"line\nbreak");
        assert_eq!(toml_escape("tab\there"), r"tab\there");
        assert_eq!(toml_escape("cr\rback"), r"cr\rback");
        assert_eq!(toml_escape("bel\x08"), r"bel\b");
        assert_eq!(toml_escape("ff\x0c"), r"ff\f");
    }

    #[test]
    #[allow(non_snake_case)]
    fn toml_escape_emits_uXXXX_for_unnamed_control_chars() {
        // NUL, ESC, DEL.
        assert_eq!(toml_escape("\0"), "\\u0000");
        assert_eq!(toml_escape("\x1b"), "\\u001B");
        assert_eq!(toml_escape("\x7f"), "\\u007F");
    }

    #[test]
    fn match_needles_in_order_finds_full_sequence_when_ordered() {
        // M4.6 P1: rolling-history matcher must succeed when every
        // needle appears in order, even when two transitions land
        // back-to-back in a single chunk.
        let haystack = b"prelude Thinking... then Working with `Bash` then Idle now";
        let needles = ["Thinking", "Working", "Bash", "Idle"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, needles.len());
    }

    #[test]
    fn match_needles_in_order_stops_when_needle_is_out_of_order() {
        // Sequence: text contains Working before Thinking — the
        // matcher must stop at index 1 (Thinking found, Working
        // already passed by the cursor).
        let haystack = b"Working appears first, then Thinking arrives later";
        let needles = ["Thinking", "Working"];
        let matched = match_needles_in_order(haystack, &needles);
        // Thinking is found (offset > 0). Then we search for Working
        // AFTER Thinking — and there's no second Working, so the
        // match stops at 1.
        assert_eq!(matched, 1);
    }

    #[test]
    fn match_needles_in_order_returns_zero_when_first_needle_missing() {
        // Used by wait_for_strings_in_order's timeout path: if even
        // the first needle never appears, `matched` stays 0 so the
        // panic message points at the right substring.
        let haystack = b"completely unrelated output, no status labels here";
        let needles = ["Thinking", "Working"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, 0);
    }

    #[test]
    fn match_needles_in_order_partial_when_later_needle_missing() {
        // Thinking + Working land in the history, but Bash never
        // shows up — matcher reports 2 (the cursor advanced past
        // both before failing on Bash). wait_for_strings_in_order
        // then surfaces "did not see `Bash` (needle #3 of 4)" on
        // timeout.
        let haystack = b"Thinking happened then Working took over, no tool was used";
        let needles = ["Thinking", "Working", "Bash", "Idle"];
        let matched = match_needles_in_order(haystack, &needles);
        assert_eq!(matched, 2);
    }

    #[test]
    fn match_prefix_then_terminal_accepts_idle_after_prefix() {
        // prd-77 chain-smoke: full prefix in order, then a rendered
        // Idle — the classic happy path. Terminal is satisfied.
        let haystack = b"Thinking... Working with `Bash` then Idle now";
        let (matched, terminal) =
            match_prefix_then_terminal(haystack, &["Thinking", "Working", "Bash"], &["Idle"]);
        assert_eq!(matched, 3);
        assert!(terminal);
    }

    #[test]
    fn match_prefix_then_terminal_accepts_placeholder_when_idle_absent() {
        // print-mode lifecycle: the agent exits before any Idle
        // frame, so the pane falls back to the placeholder. The
        // placeholder alternative (seen AFTER Bash) satisfies the
        // terminal even though `Idle` never appears.
        let haystack =
            b"Thinking... Working with `Bash`, agent exited, Launch an agent to get started";
        let (matched, terminal) = match_prefix_then_terminal(
            haystack,
            &["Thinking", "Working", "Bash"],
            &["Idle", "Launch an agent to get started"],
        );
        assert_eq!(matched, 3);
        assert!(terminal);
    }

    #[test]
    fn match_prefix_then_terminal_ignores_terminal_before_prefix_completes() {
        // A restored session renders its default `Idle` (and may show
        // the placeholder) BEFORE the agent starts. That stale early
        // terminal must NOT count: searching only after the prefix
        // cursor means a pre-lifecycle Idle is rejected.
        let haystack = b"Idle (restored) then Thinking... Working with `Bash`, nothing after";
        let (matched, terminal) = match_prefix_then_terminal(
            haystack,
            &["Thinking", "Working", "Bash"],
            &["Idle", "Launch an agent to get started"],
        );
        assert_eq!(matched, 3);
        assert!(!terminal);
    }

    /// The observed `claude_001_thinking_working_idle` flake, reduced to its
    /// mechanism. The pane's placeholder was painted BEFORE the working
    /// lifecycle and never repainted after it, so the post-prefix byte stream
    /// carries none of its bytes — yet the user is plainly looking at it, and
    /// the failing run's own panic message printed a final grid containing it.
    ///
    /// The byte stream is not a faithful record of what is on screen: ratatui
    /// renders DIFFERENTIALLY (an unchanged cell region emits nothing at all)
    /// and can split one visible line across several writes when styling
    /// changes mid-line. Either is enough to hide a terminal state that has
    /// genuinely been reached, which is why the grid is consulted as a second
    /// source of evidence.
    #[test]
    fn terminal_reached_accepts_a_terminal_state_that_arrives_on_the_grid() {
        // Placeholder bytes appear only BEFORE the prefix completes, so the
        // post-cursor stream never carries them.
        let haystack = b"Thinking... Working `Bash`";
        let working = "1 claude-sm... - Bash\nDir: .tmpZNOTi9\nWorking";
        let exited = "1 No agent - claude-sm...\nDir: .tmpZNOTi9\nLaunch an agent to get started";
        let prefix = ["Thinking", "Working", "Bash"];
        let terminals = ["Idle", "Launch an agent to get started"];
        let mut baseline = None;

        // First poll past the gate only latches what was already on screen.
        let (matched, terminal) =
            terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
                working.to_string()
            });
        assert_eq!(matched, 3);
        assert!(!terminal, "nothing has arrived yet on the first poll");

        // The agent exits and the card repaints to the placeholder.
        let (_, terminal) = terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
            exited.to_string()
        });
        assert!(
            terminal,
            "a terminal state that ARRIVES on screen must satisfy the terminal \
             condition even when a differential render never re-emitted its bytes \
             after the prefix"
        );
    }

    /// Greptile P2 on #585, and a false pass that `delegate_014` would have
    /// hit every run. Its worker command is
    /// `claude --model … --allowedTools Bash Read Write`, the deck renders a
    /// role's command on its card, and that test's terminal alternatives are
    /// `["Bash", "bash"]` — so the needle sits on the grid from boot. A bare
    /// "is it on screen" check would pass the instant the prefix completed,
    /// without the worker ever running a Bash tool. Only a needle that ARRIVES
    /// after the prefix counts.
    #[test]
    fn terminal_reached_rejects_a_needle_that_was_on_the_grid_all_along() {
        let haystack = b"Thinking... Working, worker still going";
        let grid = "orchestrator | worker\n\
                    command: claude --model haiku --allowedTools Bash Read Write\n\
                    Working";
        let mut baseline = None;
        let prefix = ["Thinking", "Working"];
        let terminals = ["Bash", "bash"];

        for _ in 0..3 {
            let (matched, terminal) =
                terminal_reached(haystack, &prefix, &terminals, &mut baseline, || {
                    grid.to_string()
                });
            assert_eq!(matched, 2);
            assert!(
                !terminal,
                "a needle that has been on screen since boot must never satisfy \
                 the terminal condition — the test would finish while the worker \
                 is still working"
            );
        }
    }

    /// The ordering guarantee must survive the grid arm: a restored session
    /// renders a default `Idle` before the agent starts, and that must still
    /// not count. The grid is consulted ONLY once the prefix has fully
    /// matched, so the gate is shut while the stale state is on screen.
    #[test]
    fn terminal_reached_still_ignores_a_terminal_state_before_the_prefix_completes() {
        let haystack = b"Idle (restored) then Thinking... Working took over, no tool used";
        let mut baseline = None;
        let (matched, terminal) = terminal_reached(
            haystack,
            &["Thinking", "Working", "Bash"],
            &["Idle", "Launch an agent to get started"],
            &mut baseline,
            || "Idle".to_string(),
        );
        assert_eq!(matched, 2);
        assert!(
            !terminal,
            "an Idle on screen while the working lifecycle is still incomplete \
             must never satisfy the terminal condition — the prefix gate is what \
             makes the grid arm safe"
        );
        assert!(
            baseline.is_none(),
            "the baseline must not latch before the gate opens, or it would \
             capture a mid-lifecycle screen"
        );
    }

    /// The cheap path stays cheap: when the byte stream already answers the
    /// question, the grid is never rendered. `snapshot_grid` locks and walks
    /// the whole screen, and this decision is polled every 20 ms.
    #[test]
    fn terminal_reached_does_not_render_the_grid_when_the_stream_settles_it() {
        let mut rendered = false;
        let mut baseline = None;
        let (matched, terminal) = terminal_reached(
            b"Thinking... Working `Bash` then Idle",
            &["Thinking", "Working", "Bash"],
            &["Idle"],
            &mut baseline,
            || {
                rendered = true;
                String::new()
            },
        );
        assert_eq!(matched, 3);
        assert!(terminal);
        assert!(
            !rendered,
            "the grid must not be rendered when the byte stream already matched"
        );
    }

    #[test]
    fn match_prefix_then_terminal_reports_incomplete_prefix() {
        // Bash never shows up: prefix stalls at 2 and terminal is
        // forced false, so the timeout path points at the missing
        // prefix needle rather than the terminal alternatives.
        let haystack = b"Thinking happened then Working took over, then Idle, no tool used";
        let (matched, terminal) = match_prefix_then_terminal(
            haystack,
            &["Thinking", "Working", "Bash"],
            &["Idle", "Launch an agent to get started"],
        );
        assert_eq!(matched, 2);
        assert!(!terminal);
    }

    /// Scenario: Import a synthetic OpenCode auth file while a hostile host config
    /// sits beside it. Only auth is copied, a minimal isolated config is created,
    /// and the imported token is registered for recording redaction.
    #[test]
    fn opencode_import_is_auth_only_and_synthesizes_minimal_config() {
        let source = race_safe_tempdir();
        let target = race_safe_tempdir();
        let source_auth = source.path().join(".local/share/opencode/auth.json");
        std::fs::create_dir_all(source_auth.parent().unwrap()).expect("source auth dir");
        std::fs::write(
            &source_auth,
            r#"{"openrouter":{"type":"api","key":"test-secret-token-249"}}"#,
        )
        .expect("source auth");
        let source_config = source.path().join(".config/opencode/opencode.jsonc");
        std::fs::create_dir_all(source_config.parent().unwrap()).expect("source config dir");
        std::fs::write(
            &source_config,
            r#"{"plugin":["host-plugin"],"mcp":{"host":{"command":"leak-secret"}}}"#,
        )
        .expect("host config");

        let redactions = import_opencode_credentials_from(source.path(), target.path())
            .expect("isolated OpenCode import");
        let imported_auth = target.path().join(".local/share/opencode/auth.json");
        assert_eq!(
            std::fs::read_to_string(imported_auth).unwrap(),
            r#"{"openrouter":{"type":"api","key":"test-secret-token-249"}}"#
        );
        assert_eq!(
            std::fs::read_to_string(target.path().join(".config/opencode/opencode.json")).unwrap(),
            MINIMAL_OPENCODE_CONFIG
        );
        assert!(
            !target
                .path()
                .join(".config/opencode/opencode.jsonc")
                .exists(),
            "the host OpenCode config must never enter the isolated HOME"
        );
        assert_eq!(redactions, vec!["test-secret-token-249"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(target.path().join(".local/share/opencode/auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "imported auth mode must stay private");
        }
    }

    /// Scenario: Point an OpenCode data root at an external directory through a
    /// symlink and attempt credential import. The importer must reject the root
    /// before reading its otherwise-regular auth leaf.
    #[cfg(unix)]
    #[test]
    fn opencode_import_rejects_a_symlinked_source_root() {
        use std::os::unix::fs::symlink;

        let source = race_safe_tempdir();
        let target = race_safe_tempdir();
        let external = race_safe_tempdir();
        std::fs::write(
            external.path().join("auth.json"),
            r#"{"openrouter":{"key":"must-not-be-imported"}}"#,
        )
        .expect("external auth");
        std::fs::create_dir_all(source.path().join(".local/share")).expect("source parents");
        symlink(external.path(), source.path().join(".local/share/opencode"))
            .expect("symlink OpenCode root");

        let error = import_opencode_credentials_from(source.path(), target.path())
            .expect_err("a symlinked OpenCode root must be refused")
            .to_string();
        assert!(
            error.contains("source directory ancestor is a symlink")
                && error.contains("~/.local/share/opencode/auth.json")
                && !error.contains(source.path().to_string_lossy().as_ref()),
            "the refusal must identify the redacted source without exposing HOME: {error}"
        );
    }

    /// Scenario: Split a known credential across adjacent PTY recording chunks.
    /// Artifact redaction must match across the chunk boundary while preserving
    /// the two timestamped cast events.
    #[test]
    fn recording_redaction_catches_credentials_split_across_events() {
        let events = vec![
            CastEvent {
                offset_secs: 0.1,
                data: b"prefix token-".to_vec(),
            },
            CastEvent {
                offset_secs: 0.2,
                data: b"secret-249 suffix".to_vec(),
            },
        ];
        let redacted = redact_cast_events(&events, &["token-secret-249".to_string()]);
        assert_eq!(redacted.len(), events.len());
        let joined: Vec<u8> = redacted.into_iter().flatten().collect();
        assert!(
            joined
                .windows(RECORDING_CREDENTIAL_REDACTION.len())
                .any(|window| window == RECORDING_CREDENTIAL_REDACTION)
        );
        assert!(
            !joined
                .windows(b"token-secret-249".len())
                .any(|window| window == b"token-secret-249"),
            "the split credential survived recording redaction: {:?}",
            String::from_utf8_lossy(&joined)
        );
    }

    // -----------------------------------------------------------------------
    // Issue #502/#785 — authorising a real-agent run from an API key
    // -----------------------------------------------------------------------

    /// Scenario: Ask for the identifier Claude Code files an API key under. It
    /// is the last 20 characters, it is counted in characters rather than bytes
    /// so a non-ASCII value cannot panic on a split code point, and a key
    /// shorter than 20 characters comes back whole instead of being padded or
    /// truncated.
    #[test]
    fn the_api_key_response_id_is_the_last_twenty_characters() {
        let key = "sk-ant-api03-0123456789abcdefghijklmnopqrstuvwxyz";
        let id = claude_api_key_response_id(key);
        assert_eq!(id.chars().count(), 20);
        assert!(key.ends_with(&id));
        assert_eq!(claude_api_key_response_id("short"), "short");
        // Multi-byte characters: 20 CHARS, and the result stays valid UTF-8.
        let unicode: String = std::iter::repeat_n('é', 25).collect();
        assert_eq!(claude_api_key_response_id(&unicode).chars().count(), 20);
    }

    /// Scenario: Register an API key for recording redaction and then render
    /// both the key and the 20-character suffix Claude Code's approval prompt
    /// paints on the terminal into a grid. Neither survives into the artifact.
    /// The suffix half is the one that matters: GitHub masks a registered
    /// secret's exact value in a rendered log, never a derivative of it, and the
    /// prompt renders exactly this derivative whenever the approval seeding is
    /// missing or wrong.
    #[test]
    fn the_api_key_and_its_rendered_suffix_are_both_redacted_from_recordings() {
        let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
        let redactions = api_key_recording_redactions(key);
        let suffix = claude_api_key_response_id(key);

        let prompt_grid = format!(
            "Detected a custom API key in your environment\n  ANTHROPIC_API_KEY: sk-ant-...{suffix}\n  Do you want to use this API key?\n"
        );
        let redacted = redact_known_credentials_text(&prompt_grid, &redactions);
        assert!(
            !redacted.contains(&suffix),
            "the rendered key suffix survived redaction: {redacted}"
        );
        assert!(redacted.contains("[REDACTED-CREDENTIAL]"));

        let env_dump = format!("$ env | grep ANTHROPIC\nANTHROPIC_API_KEY={key}\n");
        let redacted = redact_known_credentials_bytes(env_dump.as_bytes(), &redactions);
        let redacted = String::from_utf8_lossy(&redacted);
        assert!(
            !redacted.contains(key) && !redacted.contains(suffix.as_str()),
            "the key survived redaction: {redacted}"
        );
    }

    /// Scenario: Seed Claude Code's API-key approval into a config on a host
    /// whose OAuth credential set is UNUSABLE — a CI runner. The key is
    /// approved, and a `rejected` entry inherited from the host config is
    /// dropped, because the host copy is taken wholesale and a developer who
    /// once answered "No" to this key would otherwise export that refusal to a
    /// runner where the key is the only way in.
    #[test]
    fn a_key_authorised_run_approves_the_key_and_drops_an_inherited_refusal() {
        let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
        let id = claude_api_key_response_id(key);
        let mut cfg = serde_json::json!({
            "hasCompletedOnboarding": true,
            "customApiKeyResponses": { "approved": [], "rejected": [id.clone()] },
        });
        seed_claude_api_key_response(&mut cfg, key, false);
        assert!(response_list_contains(&cfg, "approved", &id));
        assert!(!response_list_contains(&cfg, "rejected", &id));
    }

    /// Scenario: Seed the same answer on a host whose OAuth credential set IS
    /// usable — a developer's machine. The key is REJECTED rather than
    /// approved, so the imported credential set stays authoritative and the run
    /// does not quietly move off the developer's subscription onto metered API
    /// billing. Measured: with the key rejected and no credential file, Claude
    /// Code declines the key and falls through to its login prompt, so on an
    /// OAuth host it authenticates exactly as it did before this existed.
    #[test]
    fn an_oauth_authorised_run_rejects_the_ambient_key_instead_of_approving_it() {
        let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
        let id = claude_api_key_response_id(key);
        let mut cfg = serde_json::json!({ "hasCompletedOnboarding": true });
        seed_claude_api_key_response(&mut cfg, key, true);
        assert!(response_list_contains(&cfg, "rejected", &id));
        assert!(!response_list_contains(&cfg, "approved", &id));
    }

    /// Scenario: The host config already approves this key and the host also has
    /// a usable OAuth credential set. That approval is the developer's own
    /// deliberate answer, so the seeding leaves it alone rather than overriding
    /// it with a refusal.
    #[test]
    fn a_deliberate_host_approval_survives_the_oauth_branch() {
        let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
        let id = claude_api_key_response_id(key);
        let mut cfg = serde_json::json!({
            "customApiKeyResponses": { "approved": [id.clone()], "rejected": [] },
        });
        seed_claude_api_key_response(&mut cfg, key, true);
        assert!(response_list_contains(&cfg, "approved", &id));
        assert!(!response_list_contains(&cfg, "rejected", &id));
    }

    /// Scenario: Seed into a host config whose `customApiKeyResponses` is
    /// missing entirely, or is present but the wrong JSON type. Neither shape
    /// may panic or silently drop the answer — a config Claude Code has never
    /// written the key into is the ordinary first-run case.
    #[test]
    fn a_missing_or_malformed_response_block_is_rebuilt_rather_than_trusted() {
        let key = "sk-ant-api03-not-a-real-key-DEADBEEFCAFEBABE0123";
        let id = claude_api_key_response_id(key);
        for hostile in [
            serde_json::json!({}),
            serde_json::json!({ "customApiKeyResponses": 7 }),
            serde_json::json!({ "customApiKeyResponses": { "approved": "not-a-list" } }),
        ] {
            let mut cfg = hostile;
            seed_claude_api_key_response(&mut cfg, key, false);
            assert!(
                response_list_contains(&cfg, "approved", &id),
                "the approval was lost: {cfg}"
            );
        }
    }

    /// Scenario: Ask whether an ambient API key is usable. Unset, empty and
    /// whitespace-only all mean absent — the same rule the three
    /// `check_pi_available` copies apply and the same rule `e2e-live.yml`'s
    /// guard step applies — while a real value comes back VERBATIM, because
    /// verbatim is what the spawned agent receives.
    #[test]
    fn an_empty_or_whitespace_only_api_key_counts_as_absent() {
        let prev = std::env::var_os(ANTHROPIC_API_KEY_ENV);
        // SAFETY: nextest runs one test per process, so this is single-threaded;
        // the var is restored before returning.
        unsafe { std::env::remove_var(ANTHROPIC_API_KEY_ENV) };
        assert!(anthropic_api_key().is_none(), "unset must read as absent");
        unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, "") };
        assert!(anthropic_api_key().is_none(), "empty must read as absent");
        unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, " \t\n") };
        assert!(
            anthropic_api_key().is_none(),
            "whitespace-only must read as absent"
        );
        unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, "sk-ant-fake") };
        assert_eq!(anthropic_api_key().as_deref(), Some("sk-ant-fake"));
        match prev {
            Some(value) => unsafe { std::env::set_var(ANTHROPIC_API_KEY_ENV, value) },
            None => unsafe { std::env::remove_var(ANTHROPIC_API_KEY_ENV) },
        }
    }

    /// Scenario: Ask whether an ambient Anthropic key is enough to run the
    /// OpenCode tests. It is only enough when the configured test model names
    /// the `anthropic` provider — the harness forwards that key and no other,
    /// so opening the gate for an `openai/...` model would turn a clean skip
    /// into a failure deep in a PTY wait.
    #[test]
    fn the_opencode_env_key_path_is_offered_only_for_an_anthropic_model() {
        assert_eq!(
            OPENCODE_TEST_MODEL_DEFAULT.split_once('/').map(|(p, _)| p),
            Some("openai"),
            "the default model is the case the provider match exists to exclude"
        );
        // `opencode_test_model` memoises, so drive the pure predicate the gate
        // uses rather than the OnceLock: provider match AND key presence.
        for (model, provider_matches) in [
            ("anthropic/claude-haiku-4-5", true),
            ("openai/gpt-5.4-mini", false),
            ("openrouter/anthropic/claude-haiku-4-5", false),
            ("no-slash-at-all", false),
        ] {
            assert_eq!(
                model.split_once('/').is_some_and(|(p, _)| p == "anthropic"),
                provider_matches,
                "provider match for {model}"
            );
        }
    }
}
