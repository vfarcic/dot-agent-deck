//! PRD #2050: how the OpenCode in a deck pane actually runs, read from the
//! pane's process tree, and the card notice ([`crate::agent_notice`]) for a way
//! the deck cannot fully support.
//!
//! Why the process tree: the deck starts a command it can see into with
//! `--standalone` ([`crate::opencode_version::with_standalone`]), but a wrapper
//! (`devbox run oc-big`, a script) hides which OpenCode runs and how. What runs
//! is visible from outside, in the same shape for the full TUI and for
//! `opencode mini` (which loads no TUI plugin, so nothing inside OpenCode
//! could report it there). Measured on OpenCode 2.2.5, with each started
//! through a wrapper script on a terminal:
//!
//! - **standalone** (`--standalone`, or the service disabled with `opencode
//!   service set disabled true`): the client (`opencode`, `opencode mini`) has
//!   a child `opencode serve --stdio --port 0`, its private server, within
//!   about half a second of starting;
//! - **shared service**: the client has no such child. The first client on the
//!   machine has the detached `opencode serve --service` it started as a child
//!   until that client exits; a later one has no OpenCode child at all.
//!
//! On OpenCode 1.x the plugins run inside the client, so the test is never
//! applied to a 1.x client.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent_notice::{AgentNotice, OPENCODE_SUPPORTED_FLOOR};
use crate::opencode_version::{Major, Version};

/// How often the daemon looks at OpenCode panes' process trees.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long an OpenCode 2 client must have been seen without a private server
/// before its card says it is on the shared service. The private server was
/// measured to appear within about half a second; the margin covers a loaded
/// machine.
pub const SHARED_GRACE: Duration = Duration::from_secs(10);

/// How long an OpenCode client must have been seen with no sign of the deck's
/// plugin before its card says the integration is not working. The plugin
/// arms the deck's message receiver as it loads (the TUI plugin in the full
/// TUI, the server plugin under `mini`), within seconds of OpenCode starting,
/// and keeps it armed while OpenCode runs, idle or not.
pub const SILENT_GRACE: Duration = Duration::from_secs(45);

/// One process, as much of it as the sample read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    /// The executable it runs, when it could be read.
    pub exe: Option<PathBuf>,
    /// Its arguments, read only for an OpenCode process.
    pub argv: Option<Vec<String>>,
}

/// The OpenCode client a pane runs: its process, its binary, and whether it
/// has a private server of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub pid: i32,
    pub exe: Option<PathBuf>,
    pub private_server: bool,
}

/// OpenCode subcommands that are not an agent session (1.x and 2.x `--help`).
/// A client running one of them is not the pane's agent.
const NOT_A_SESSION: &[&str] = &[
    "serve",
    "web",
    "acp",
    "api",
    "debug",
    "auth",
    "mcp",
    "plugin",
    "models",
    "stats",
    "service",
    "reload",
    "pair",
    "upgrade",
    "update",
    "uninstall",
    "providers",
    "agent",
    "export",
    "import",
    "github",
    "generate",
    "completion",
];

/// Whether `name` (a file name) names an OpenCode binary: `opencode`,
/// `opencode2`, `opencode.exe`, `opencode-1.18.35`
/// ([`crate::opencode_version::opencode_name`]).
pub fn opencode_name(name: &str) -> bool {
    crate::opencode_version::opencode_name(name)
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// Whether `row` runs an OpenCode binary: by its executable, or by its first
/// argument when the executable could not be read.
pub fn is_opencode(row: &ProcRow) -> bool {
    match &row.exe {
        Some(exe) => file_name(exe).is_some_and(opencode_name),
        None => row
            .argv
            .as_ref()
            .and_then(|argv| argv.first())
            .and_then(|first| file_name(Path::new(first)))
            .is_some_and(opencode_name),
    }
}

/// The OpenCode subcommand `row` runs, if its arguments say one: the first
/// word after the program that is not a flag or a flag's value, since flags
/// may come first (`opencode --auto mini`).
fn subcommand(row: &ProcRow) -> Option<&str> {
    let args = row.argv.as_ref()?.get(1..)?;
    crate::opencode_version::first_positional(args).map(|n| args[n].as_str())
}

/// Whether `argv` (a process's arguments, program first) carries `serve` and
/// `--stdio` as words after the program: an OpenCode private server's
/// arguments (`serve --stdio --port 0`, measured on 2.2.5). Matched as words
/// anywhere, not by position, so a program path that `ps` split on its
/// spaces (macOS) does not hide them.
pub fn serves_stdio(argv: &[String]) -> bool {
    let args = argv.get(1..).unwrap_or_default();
    args.iter().any(|a| a == "serve") && args.iter().any(|a| a == "--stdio")
}

/// Whether `row` is an OpenCode private server (`serve --stdio`).
fn private_server(row: &ProcRow) -> bool {
    is_opencode(row) && row.argv.as_deref().is_some_and(serves_stdio)
}

/// Whether process `pid`'s stdin and stdout are both sockets. An OpenCode 2
/// private server's are, a socketpair with its client (measured on 2.2.5),
/// while a process a tool started keeps the tool's pipes or `/dev/null`.
/// Read from `/proc`; `false` when either cannot be read.
#[cfg(target_os = "linux")]
pub fn stdio_is_sockets(pid: u32) -> bool {
    [0, 1].iter().all(|fd| {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"))
    })
}

/// Off Linux there is no `/proc` to read a process's descriptors from, so
/// this is not checked: always `true`.
#[cfg(not(target_os = "linux"))]
pub fn stdio_is_sockets(_pid: u32) -> bool {
    true
}

/// `root` and every process below it in `rows`, breadth first, each once.
pub fn subtree(rows: &[ProcRow], root: i32) -> Vec<&ProcRow> {
    let mut children: HashMap<i32, Vec<&ProcRow>> = HashMap::new();
    for row in rows {
        children.entry(row.ppid).or_default().push(row);
    }
    let mut out: Vec<&ProcRow> = rows.iter().filter(|r| r.pid == root).collect();
    let mut seen: HashSet<i32> = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        for kid in children.get(&pid).into_iter().flatten() {
            if seen.insert(kid.pid) {
                out.push(kid);
                queue.push_back(kid.pid);
            }
        }
    }
    out
}

/// The OpenCode session client in the pane rooted at `root`: the first
/// OpenCode process (breadth first) whose parent is not an OpenCode process
/// and whose subcommand is a session's. `None` when the pane runs none.
pub fn find_client(rows: &[ProcRow], root: i32) -> Option<Client> {
    let tree = subtree(rows, root);
    let opencode_pids: HashSet<i32> = tree
        .iter()
        .filter(|r| is_opencode(r))
        .map(|r| r.pid)
        .collect();
    let client = tree.iter().find(|row| {
        is_opencode(row)
            && !opencode_pids.contains(&row.ppid)
            && !subcommand(row).is_some_and(|sub| NOT_A_SESSION.contains(&sub))
    })?;
    Some(Client {
        pid: client.pid,
        exe: client.exe.clone(),
        private_server: tree
            .iter()
            .any(|row| row.ppid == client.pid && private_server(row)),
    })
}

/// The notice an OpenCode client's card carries, or `None` when the deck
/// supports how it runs (or it is too early to tell).
///
/// - `version`: what the client's binary reported, `None` when unknown.
/// - `seen_for`: how long the deck has been seeing this client.
/// - `reporting`: the deck's plugin has shown it runs for this agent (a hook
///   event from it, or its message receiver armed).
///
/// An unsupported version is said at once; the shared service after
/// [`SHARED_GRACE`]; a silent plugin after [`SILENT_GRACE`], and never while
/// one of the other two applies, since they explain the silence.
pub fn notice_for(
    client: &Client,
    version: Option<Version>,
    seen_for: Duration,
    reporting: bool,
) -> Option<AgentNotice> {
    if let Some(v) = version
        && v < OPENCODE_SUPPORTED_FLOOR
    {
        return Some(AgentNotice::opencode_unsupported_version(v));
    }
    if version.map(Major::of) == Some(Major::V2) && !client.private_server {
        return (seen_for >= SHARED_GRACE).then(AgentNotice::opencode_shared_service);
    }
    (!reporting && seen_for >= SILENT_GRACE).then(AgentNotice::opencode_not_reporting)
}

/// One sample of the processes below `roots`: the whole process table's
/// pids and parents, the executable of each process below a root, and the
/// arguments of each of those that is an OpenCode binary — so a pane's build
/// tools never have their arguments read. Blocking; `None` when the table
/// cannot be read on this platform.
pub fn sample(roots: &[i32]) -> Option<Vec<ProcRow>> {
    let table = crate::platform::proc::process_table(&[])?;
    let mut rows: Vec<ProcRow> = table
        .iter()
        .map(|info| ProcRow {
            pid: info.pid,
            ppid: info.ppid,
            exe: None,
            argv: None,
        })
        .collect();
    let mut below: HashSet<i32> = HashSet::new();
    for root in roots {
        below.extend(subtree(&rows, *root).iter().map(|r| r.pid));
    }
    for row in rows.iter_mut().filter(|r| below.contains(&r.pid)) {
        let Ok(pid) = u32::try_from(row.pid) else {
            continue;
        };
        row.exe = crate::platform::proc::executable_of(pid);
        if row
            .exe
            .as_deref()
            .and_then(file_name)
            .is_some_and(opencode_name)
        {
            row.argv = arguments_of(pid);
        }
    }
    Some(rows)
}

/// Process `pid`'s arguments.
#[cfg(target_os = "linux")]
pub(crate) fn arguments_of(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

/// Process `pid`'s arguments, split on whitespace (`ps` joins them), which is
/// enough for the words this module looks for, matched as words
/// ([`serves_stdio`]). The `ps` runs off the async executor
/// ([`crate::inbox_hub::off_the_executor`]): the daemon's receiver admission
/// reaches it from a connection handler. Untested here: no macOS host.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn arguments_of(pid: u32) -> Option<Vec<String>> {
    let out = crate::inbox_hub::off_the_executor(|| {
        std::process::Command::new("ps")
            .args(["-o", "args=", "-p", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
    })
    .ok()?;
    let argv: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    (!argv.is_empty()).then_some(argv)
}

#[cfg(not(unix))]
pub(crate) fn arguments_of(_pid: u32) -> Option<Vec<String>> {
    None
}

/// One OpenCode agent the daemon watches: its pane, its registry id and the
/// pid its process tree hangs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watched {
    pub pane_id: String,
    pub agent_id: String,
    pub root_pid: i32,
}

/// What the daemon remembers between samples: when it first saw each agent's
/// current client, so a notice that needs time waits for it, and a new client
/// (OpenCode restarted inside the pane) starts the wait again.
#[derive(Debug, Default)]
pub struct Watch {
    first_seen: HashMap<String, (i32, std::time::Instant)>,
}

impl Watch {
    /// How long `agent_id`'s client `client_pid` has been seen as of `now`,
    /// remembering it from now if it is new.
    pub fn seen_for(
        &mut self,
        agent_id: &str,
        client_pid: i32,
        now: std::time::Instant,
    ) -> Duration {
        let entry = self
            .first_seen
            .entry(agent_id.to_string())
            .or_insert((client_pid, now));
        if entry.0 != client_pid {
            *entry = (client_pid, now);
        }
        now.saturating_duration_since(entry.1)
    }

    /// Forget every agent except `keep`.
    pub fn retain(&mut self, keep: &HashSet<&str>) {
        self.first_seen.retain(|id, _| keep.contains(id.as_str()));
    }

    /// Forget `agent_id`'s client (the pane runs none now).
    pub fn forget(&mut self, agent_id: &str) {
        self.first_seen.remove(agent_id);
    }
}

/// How long before the deck first saw a client its plugin's last sign may be
/// and still count for that client: the plugin can report as OpenCode
/// starts, up to a poll before the first sample sees the process.
pub const REPORT_SLACK: Duration = Duration::from_secs(10);

/// One look at every watched agent: the notice each one's card should carry
/// now, from the process sample `rows`.
///
/// - `version_of` gives a client binary's version. In production it is
///   [`crate::opencode_version::recorded_version`], which only reads what the
///   spawn-time probe recorded and runs nothing, so a client the deck did not
///   probe has an unknown version (and gets neither the shared-service nor
///   the unsupported-version notice).
/// - `last_report` gives, per agent, when its plugin last showed it runs (a
///   receiver registered, or a native hook event). It counts only when it is
///   no older than [`REPORT_SLACK`] before the deck first saw the agent's
///   current client, so a sign from an earlier OpenCode in the same pane does
///   not vouch for one restarted with a broken plugin (review N1).
pub fn evaluate(
    watched: &[Watched],
    rows: &[ProcRow],
    watch: &mut Watch,
    now: std::time::Instant,
    version_of: &dyn Fn(&Path) -> Option<Version>,
    last_report: &HashMap<String, std::time::Instant>,
) -> Vec<(Watched, Option<AgentNotice>)> {
    let keep: HashSet<&str> = watched.iter().map(|w| w.agent_id.as_str()).collect();
    watch.retain(&keep);
    watched
        .iter()
        .map(|w| {
            let notice = match find_client(rows, w.root_pid) {
                None => {
                    watch.forget(&w.agent_id);
                    None
                }
                Some(client) => {
                    let seen_for = watch.seen_for(&w.agent_id, client.pid, now);
                    let version = client.exe.as_deref().and_then(version_of);
                    let first_seen = now.checked_sub(seen_for + REPORT_SLACK);
                    let reporting = last_report
                        .get(&w.agent_id)
                        .is_some_and(|&at| first_seen.is_none_or(|from| at >= from));
                    notice_for(&client, version, seen_for, reporting)
                }
            };
            (w.clone(), notice)
        })
        .collect()
}

/// The daemon's watch: every [`POLL_INTERVAL`], sample the process trees of
/// the agents [`crate::agent_pty::AgentPtyRegistry::opencode_watch_list`]
/// names and set each one's card notice
/// ([`crate::inbox_hub::InboxHub::set_agent_notice`], which tells the clients
/// only of a change). Takes no sample while no OpenCode agent runs. One
/// sample at a time: a sample that stalls (a `ps` stuck on a hung filesystem)
/// stalls this watch and nothing else, and never starts a second one. It
/// never runs a binary it finds: versions come only from the spawn-time
/// probe's record ([`crate::opencode_version::recorded_version`]).
pub async fn run_monitor(
    registry: std::sync::Arc<crate::agent_pty::AgentPtyRegistry>,
    state: crate::state::SharedState,
) {
    let mut watch = Watch::default();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let watched = registry.opencode_watch_list();
        if watched.is_empty() {
            watch.retain(&HashSet::new());
            continue;
        }
        let mut last_report: HashMap<String, std::time::Instant> = watched
            .iter()
            .filter_map(|w| {
                registry
                    .inbox()
                    .integration_registered_at(&w.pane_id, &w.agent_id)
                    .map(|at| (w.agent_id.clone(), at))
            })
            .collect();
        {
            let state = state.read().await;
            let (now, wall) = (std::time::Instant::now(), chrono::Utc::now());
            for w in &watched {
                // The newest native hook event of this pane's agent, as an
                // instant on the monitor's clock.
                let hooked = state
                    .sessions
                    .values()
                    .filter(|session| {
                        session.native_hooks_reported
                            && session.pane_id.as_deref() == Some(w.pane_id.as_str())
                            && session
                                .agent_id
                                .as_deref()
                                .is_none_or(|agent| agent == w.agent_id)
                    })
                    .map(|session| session.last_activity)
                    .max()
                    .map(|at| {
                        let ago = (wall - at).to_std().unwrap_or_default();
                        now.checked_sub(ago).unwrap_or(now)
                    });
                if let Some(at) = hooked {
                    let entry = last_report.entry(w.agent_id.clone()).or_insert(at);
                    *entry = (*entry).max(at);
                }
            }
        }
        let job = tokio::task::spawn_blocking(move || {
            let roots: Vec<i32> = watched.iter().map(|w| w.root_pid).collect();
            let rows = sample(&roots)?;
            let notices = evaluate(
                &watched,
                &rows,
                &mut watch,
                std::time::Instant::now(),
                &crate::opencode_version::recorded_version,
                &last_report,
            );
            Some((watch, notices))
        });
        let Ok(outcome) = job.await else {
            // The blocking task panicked; start over with a fresh watch.
            watch = Watch::default();
            continue;
        };
        match outcome {
            Some((kept, notices)) => {
                watch = kept;
                for (w, notice) in notices {
                    registry
                        .inbox()
                        .set_agent_notice(&w.pane_id, &w.agent_id, notice);
                }
            }
            // No process table on this platform, or `ps` failed: no opinion,
            // so nothing is changed.
            None => watch = Watch::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_notice::AgentNoticeKind;

    fn row(pid: i32, ppid: i32, exe: &str, argv: &[&str]) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            exe: Some(PathBuf::from(exe)),
            argv: (!argv.is_empty()).then(|| argv.iter().map(|a| a.to_string()).collect()),
        }
    }

    const OC: &str = "/home/u/.opencode/bin/opencode";

    /// The measured 2.2.5 shapes: a wrapper shell running the client, with or
    /// without a private server.
    fn wrapped(client_args: &[&str], child: Option<&[&str]>) -> Vec<ProcRow> {
        let mut rows = vec![
            row(1, 0, "/sbin/init", &[]),
            row(100, 1, "/usr/bin/dash", &[]),
            row(101, 100, "/usr/bin/dash", &[]),
            row(102, 101, OC, client_args),
            row(900, 1, OC, &["opencode", "serve", "--service"]),
        ];
        if let Some(args) = child {
            rows.push(row(103, 102, OC, args));
        }
        rows
    }

    #[test]
    fn a_standalone_client_has_a_private_server_in_the_full_tui_and_in_mini() {
        for client in [
            &["opencode", "--standalone"][..],
            &["opencode", "mini", "--standalone"],
        ] {
            let rows = wrapped(
                client,
                Some(&["opencode", "serve", "--stdio", "--port", "0"]),
            );
            let found = find_client(&rows, 100).expect("the client is found below the wrapper");
            assert_eq!(found.pid, 102);
            assert!(found.private_server, "{client:?}");
        }
    }

    #[test]
    fn a_client_on_the_shared_service_has_none_even_when_it_started_the_service() {
        for child in [None, Some(&["opencode", "serve", "--service"][..])] {
            for client in [&["opencode"][..], &["opencode", "mini"]] {
                let rows = wrapped(client, child);
                let found = find_client(&rows, 100).expect("found");
                assert!(!found.private_server, "{client:?} with {child:?}");
            }
        }
    }

    #[test]
    fn a_pane_without_an_opencode_session_has_no_client() {
        let mut rows = wrapped(&["opencode", "auth", "login"], None);
        assert_eq!(find_client(&rows, 100), None, "a non-session subcommand");
        rows.retain(|r| r.pid != 102);
        assert_eq!(find_client(&rows, 100), None, "no OpenCode at all");
        // The machine's background service is not below the pane.
        assert_eq!(find_client(&rows, 1).map(|c| c.pid), None);
    }

    #[test]
    fn the_client_is_the_topmost_opencode_and_a_direct_command_is_its_own_root() {
        // A node launcher (an older npm install) running the native binary.
        let rows = vec![
            row(200, 1, "/usr/bin/node", &[]),
            row(
                201,
                200,
                "/x/node_modules/opencode-ai/bin/opencode.exe",
                &["opencode"],
            ),
        ];
        assert_eq!(find_client(&rows, 200).map(|c| c.pid), Some(201));
        let direct = wrapped(&["opencode"], None);
        assert_eq!(find_client(&direct, 102).map(|c| c.pid), Some(102));
    }

    #[test]
    fn a_process_whose_executable_could_not_be_read_is_judged_by_its_first_argument() {
        let unread = ProcRow {
            pid: 5,
            ppid: 4,
            exe: None,
            argv: Some(vec!["/opt/opencode".to_string(), "mini".to_string()]),
        };
        assert!(is_opencode(&unread));
        let other = ProcRow {
            argv: Some(vec!["bash".to_string()]),
            ..unread
        };
        assert!(!is_opencode(&other));
    }

    #[test]
    fn subtree_terminates_on_a_cycle() {
        let rows = vec![row(1, 2, "/a", &[]), row(2, 1, "/b", &[])];
        assert_eq!(subtree(&rows, 1).len(), 2);
    }

    fn client(private_server: bool) -> Client {
        Client {
            pid: 1,
            exe: None,
            private_server,
        }
    }

    fn kind(notice: Option<AgentNotice>) -> Option<AgentNoticeKind> {
        notice.map(|n| n.kind)
    }

    #[test]
    fn opencode_2_on_the_shared_service_is_reported_after_the_grace() {
        let early = notice_for(
            &client(false),
            Some((2, 2, 5)),
            Duration::from_secs(2),
            false,
        );
        assert_eq!(kind(early), None, "too early to tell");
        let late = notice_for(&client(false), Some((2, 2, 5)), SHARED_GRACE, true);
        assert_eq!(kind(late), Some(AgentNoticeKind::OpencodeSharedService));
        // Even long after, the shared service explains the silence, so the
        // card does not also say the plugin is broken.
        let silent = notice_for(&client(false), Some((2, 2, 5)), SILENT_GRACE * 2, false);
        assert_eq!(kind(silent), Some(AgentNoticeKind::OpencodeSharedService));
    }

    #[test]
    fn a_private_server_or_opencode_1_is_never_the_shared_service() {
        let standalone = notice_for(&client(true), Some((2, 2, 5)), SILENT_GRACE, true);
        assert_eq!(kind(standalone), None);
        let v1 = notice_for(&client(false), Some((1, 19, 5)), SILENT_GRACE, true);
        assert_eq!(kind(v1), None, "1.x runs plugins in the client");
        let unknown = notice_for(&client(false), None, SILENT_GRACE, true);
        assert_eq!(kind(unknown), None, "an unknown version is not called 2.x");
    }

    #[test]
    fn an_opencode_older_than_the_floor_is_reported_at_once() {
        let old = notice_for(&client(false), Some((1, 18, 28)), Duration::ZERO, true);
        assert_eq!(kind(old), Some(AgentNoticeKind::OpencodeUnsupportedVersion));
        let floor = notice_for(&client(false), Some((1, 18, 29)), Duration::ZERO, true);
        assert_eq!(kind(floor), None, "1.18.29 is supported");
        let ancient = notice_for(&client(false), Some((0, 9, 0)), Duration::ZERO, true);
        assert_eq!(
            kind(ancient),
            Some(AgentNoticeKind::OpencodeUnsupportedVersion)
        );
    }

    #[test]
    fn a_silent_plugin_is_reported_only_after_its_grace_and_only_while_silent() {
        for version in [Some((1, 19, 5)), Some((2, 2, 5)), None] {
            let starting = notice_for(&client(true), version, SILENT_GRACE / 2, false);
            assert_eq!(kind(starting), None, "{version:?}: still starting");
            let silent = notice_for(&client(true), version, SILENT_GRACE, false);
            assert_eq!(
                kind(silent),
                Some(AgentNoticeKind::OpencodeNotReporting),
                "{version:?}"
            );
            let healthy = notice_for(&client(true), version, SILENT_GRACE * 10, true);
            assert_eq!(kind(healthy), None, "{version:?}: idle but reporting");
        }
    }

    /// Scenario: two OpenCode panes are watched — one standalone whose plugin
    /// reports, one on the shared service — and a third whose OpenCode has
    /// exited. One look after the grace gives the shared one its notice and
    /// the other two none.
    #[test]
    fn evaluate_gives_each_watched_agent_its_own_notice() {
        let mut rows = wrapped(
            &["opencode", "--standalone"],
            Some(&["opencode", "serve", "--stdio"]),
        );
        rows.extend([
            row(300, 1, "/usr/bin/dash", &[]),
            row(301, 300, OC, &["opencode", "mini"]),
            row(400, 1, "/usr/bin/dash", &[]),
        ]);
        let watched = |pane: &str, root| Watched {
            pane_id: pane.to_string(),
            agent_id: format!("agent-{pane}"),
            root_pid: root,
        };
        let all = [watched("a", 100), watched("b", 300), watched("c", 400)];
        let t0 = std::time::Instant::now();
        let reporting = HashMap::from([("agent-a".to_string(), t0)]);
        let v2 = |_: &Path| Some((2, 2, 5));
        let mut watch = Watch::default();
        let first = evaluate(&all, &rows, &mut watch, t0, &v2, &reporting);
        assert!(
            first.iter().all(|(_, n)| n.is_none()),
            "nothing before the grace"
        );
        let later = evaluate(&all, &rows, &mut watch, t0 + SHARED_GRACE, &v2, &reporting);
        let kinds: Vec<_> = later
            .iter()
            .map(|(w, n)| (w.pane_id.as_str(), n.as_ref().map(|n| n.kind)))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("a", None),
                ("b", Some(AgentNoticeKind::OpencodeSharedService)),
                ("c", None),
            ]
        );
    }

    /// Scenario: an OpenCode pane whose plugin reported once, then OpenCode
    /// was restarted in the same pane with a plugin that never reports. The
    /// old sign does not vouch for the new client, so after the grace its
    /// card says the integration is not working; a sign from the new client
    /// clears it (review N1).
    #[test]
    fn a_sign_from_an_earlier_opencode_does_not_vouch_for_a_restarted_one() {
        let rows_with = |client_pid: i32| {
            vec![
                row(1, 0, "/sbin/init", &[]),
                row(100, 1, "/usr/bin/dash", &[]),
                row(client_pid, 100, OC, &["opencode", "--standalone"]),
                row(
                    client_pid + 1,
                    client_pid,
                    OC,
                    &["opencode", "serve", "--stdio"],
                ),
            ]
        };
        let watched = [Watched {
            pane_id: "a".to_string(),
            agent_id: "agent-a".to_string(),
            root_pid: 100,
        }];
        let v2 = |_: &Path| Some((2, 2, 5));
        let mut watch = Watch::default();
        let t0 = std::time::Instant::now();
        let mut last_report = HashMap::from([("agent-a".to_string(), t0)]);
        let kind_at = |watch: &mut Watch, rows: &[ProcRow], at, last: &HashMap<_, _>| {
            evaluate(&watched, rows, watch, at, &v2, last)[0]
                .1
                .as_ref()
                .map(|n| n.kind)
        };
        let first = rows_with(102);
        assert_eq!(kind_at(&mut watch, &first, t0, &last_report), None);
        assert_eq!(
            kind_at(&mut watch, &first, t0 + SILENT_GRACE * 2, &last_report),
            None,
            "idle but its plugin reported"
        );
        // OpenCode restarted in the pane, long after that sign.
        let restarted = rows_with(202);
        let t1 = t0 + SILENT_GRACE * 3;
        assert_eq!(kind_at(&mut watch, &restarted, t1, &last_report), None);
        assert_eq!(
            kind_at(&mut watch, &restarted, t1 + SILENT_GRACE, &last_report),
            Some(AgentNoticeKind::OpencodeNotReporting),
            "the earlier OpenCode's sign does not count"
        );
        last_report.insert("agent-a".to_string(), t1 + SILENT_GRACE);
        assert_eq!(
            kind_at(&mut watch, &restarted, t1 + SILENT_GRACE * 2, &last_report),
            None
        );
    }

    #[test]
    fn the_subcommand_is_found_past_leading_flags_and_serve_as_words() {
        let rows = wrapped(&["opencode", "--auto", "auth", "login"], None);
        assert_eq!(find_client(&rows, 100), None, "`auth` behind a flag");
        let rows = wrapped(&["opencode", "--log-level", "info", "mini"], None);
        assert_eq!(find_client(&rows, 100).map(|c| c.pid), Some(102));
        let split: Vec<String> = [
            "/Users/u/Application",
            "Support/oc/opencode",
            "serve",
            "--stdio",
        ]
        .iter()
        .map(|a| a.to_string())
        .collect();
        assert!(serves_stdio(&split), "a path `ps` split on its space");
        assert!(!serves_stdio(&[
            "opencode".to_string(),
            "serve".to_string()
        ]));
        assert!(!serves_stdio(
            &["serve".to_string(), "--stdio".to_string()][..1]
        ));
    }

    #[test]
    fn the_watch_restarts_the_wait_for_a_new_client() {
        let mut watch = Watch::default();
        let t0 = std::time::Instant::now();
        assert_eq!(watch.seen_for("a", 10, t0), Duration::ZERO);
        assert_eq!(
            watch.seen_for("a", 10, t0 + Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        assert_eq!(
            watch.seen_for("a", 11, t0 + Duration::from_secs(8)),
            Duration::ZERO
        );
        watch.retain(&HashSet::new());
        assert_eq!(
            watch.seen_for("a", 11, t0 + Duration::from_secs(9)),
            Duration::ZERO
        );
    }
}
