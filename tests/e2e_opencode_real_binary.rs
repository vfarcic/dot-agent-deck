#![cfg(all(feature = "e2e", target_os = "linux"))]
//! Issue #2051 — the deck's OpenCode plugin under REAL, pinned OpenCode 1.x and
//! 2.x binaries, in a deck pane, with no model.
//!
//! The stand-in tests (`hooks/install/025`-`027`, `inbox/opencode/059`, the
//! card notices) prove what the deck writes and how it reads a fake host; only
//! a real OpenCode can say whether it loads what the deck wrote. These start
//! the binaries `scripts/install-pinned-opencode.sh` installs (the versions
//! `scripts/opencode-pins/` pins, which `scripts/box/bootstrap.sh` puts on a
//! dev box too), found through the harness's pinned-binary choice
//! (`common::pinned_opencode_bin`, the `v1`/`v2` values of
//! `DOT_AGENT_DECK_OPENCODE_TEST_BIN`).
//!
//! LANE 1, and why that holds (CLAUDE.md rule 5: no e2e test reaches a real
//! agent in CI). OpenCode is started with no model it could use, and every
//! test checks that rather than assuming it ([`NoModel`]):
//!
//! - no provider: the scratch HOME's `opencode.json` sets `enabled_providers`
//!   to an empty list, which also turns off OpenCode's free built-in models
//!   (with no config both pinned versions select one, credential or not), and
//!   each pane must draw OpenCode's own `No provider selected`;
//! - no credential: the deck is launched with
//!   `TuiDeckBuilder::without_agent_credentials` on top of the harness's
//!   `env_clear`, no OpenCode auth file exists in the HOME before or after a
//!   test, and the environment of every OpenCode process a pane runs (read
//!   from `/proc`) holds no variable named like a key, token, secret, password
//!   or credential;
//! - no proxy that answers: every proxy variable points at a listener in this
//!   test that answers nothing and closes each connection, with only loopback
//!   exempt. That listener must see at least one connection (OpenCode's
//!   model-catalog fetch as it starts, measured on 1.19.5 and 2.2.5), which
//!   shows OpenCode reads the variables. It sees only what takes the proxy: a
//!   connection opened without it would go unobserved, which is why the first
//!   two checks, not this one, are what put a model out of reach.
//!
//! Nothing is typed into a pane. Where a session is needed, 1.x gets one from
//! its own loopback server (`--port`, `POST /session`), and 2.x from a deck
//! message the plugin prompts into OpenCode, whose turn then fails at once for
//! want of a model (measured on 2.2.5: `Error`, then `Idle`, with the proxy
//! seeing nothing but the catalog fetch).
//!
//! What "the plugin loaded" means here, measured on the pinned versions: the
//! deck's plugin starts `dot-agent-deck inbox wait` watchers from inside
//! OpenCode — the server half one with `--doorbell-only`, the TUI half one
//! without — and the deck then reports the pane's inbox armed by
//! `opencode_plugin`. OpenCode 2.x reports a plugin that fails to load in its
//! log (`failed to load plugin`) and on screen (`Plugin failed`), and
//! `hooks/opencode-real/003` proves that check can fire. OpenCode 1.19.5
//! reported the same broken plugin nowhere, in its log or on screen, so for
//! 1.x the watchers are the evidence.
//!
//! Linux only: processes and their environments are read from `/proc`.

dot_agent_deck_test_harness::link!();
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use common::TuiDeck;
use dot_agent_deck::event::{AgentEvent, EventType};
use serde_json::{Value, json};
use spec::spec;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bound on each wait: OpenCode's start, its plugin's registration, a session.
fn budget() -> Duration {
    common::load_scaled(Duration::from_secs(60))
}

/// OpenCode's own log directory under its data root (`XDG_DATA_HOME`), on
/// both versions.
const LOG_DIR: &str = "opencode/log";

/// The deck's server module, which both versions load (`hooks/install/025`).
const SERVER_MODULE: &str = ".config/opencode/plugin/dot-agent-deck.js";

/// The pinned OpenCode `$major`.x, or return from the test after reporting a
/// skip (a failure where CI requires the binaries).
macro_rules! pinned_or_skip {
    ($major:expr) => {
        match common::pinned_opencode_bin($major) {
            Ok(bin) => bin,
            Err(why) => {
                skip_unless!(Err(why));
                return;
            }
        }
    };
}

/// A proxy that answers nothing: it records the first line of every
/// connection (`CONNECT host:443 HTTP/1.1`) and closes it.
struct RefusingProxy {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RefusingProxy {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the refusing proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                log.lock().unwrap().push(line.trim().to_string());
                // Dropped here: closed with no answer.
            }
        });
        Self { port, seen }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// One process of the deck's HOME, from `/proc`.
#[derive(Clone, Debug)]
struct Proc {
    pid: u32,
    ppid: u32,
    exe: String,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
}

impl Proc {
    fn has_args(&self, words: &[&str]) -> bool {
        words.iter().all(|w| self.argv.iter().any(|a| a == w))
    }

    /// Whether the executable is an OpenCode binary (`opencode`,
    /// `opencode.exe`, as both pinned packages name it).
    fn is_opencode(&self) -> bool {
        Path::new(&self.exe)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("opencode"))
    }
}

/// Every process whose `HOME` is `home` (the deck's scratch HOME, which its
/// panes inherit), so concurrent tests' processes are never mistaken for ours.
fn procs(home: &Path) -> Vec<Proc> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let env: BTreeMap<String, String> = raw
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let kv = String::from_utf8_lossy(kv);
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect();
        if env.get("HOME").map(Path::new) != Some(home) {
            continue;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // `pid (comm) state ppid …`; comm may hold spaces and parentheses.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let argv = std::fs::read(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        found.push(Proc {
            pid,
            ppid,
            exe,
            argv,
            env,
        });
    }
    found
}

/// Whether `pid` is `ancestor` or descends from it among `all`.
fn descends(all: &[Proc], mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        match all.iter().find(|p| p.pid == pid) {
            Some(p) if p.ppid != 0 => pid = p.ppid,
            _ => return false,
        }
    }
    false
}

/// A started OpenCode pane.
struct Pane {
    agent: String,
    id: String,
    major: u32,
    /// The pane's `XDG_DATA_HOME`: OpenCode's sessions, auth and logs.
    data: PathBuf,
}

/// What one OpenCode pane runs, from `/proc`: the client (the topmost OpenCode
/// process of the pane), 2.x's private server, and everything else in the
/// HOME, the deck's watchers among it.
struct Tree {
    all: Vec<Proc>,
    client: Proc,
    server: Option<Proc>,
}

impl Tree {
    /// The OpenCode processes: the client and, on 2.x, its private server.
    fn opencode(&self) -> Vec<&Proc> {
        std::iter::once(&self.client)
            .chain(self.server.as_ref())
            .collect()
    }

    /// The server half's watcher: a `dot-agent-deck inbox wait --stream
    /// --doorbell-only` in the process that hosts server plugins (1.x's single
    /// process, 2.x's private server).
    fn server_watcher(&self) -> Option<&Proc> {
        let host = self.server.as_ref().unwrap_or(&self.client);
        self.watchers(true)
            .find(|p| descends(&self.all, p.ppid, host.pid))
    }

    /// The TUI half's watcher: the one without `--doorbell-only`, in the
    /// client and, on 2.x, not in its private server.
    fn tui_watcher(&self) -> Option<&Proc> {
        self.watchers(false).find(|p| {
            descends(&self.all, p.ppid, self.client.pid)
                && !self
                    .server
                    .as_ref()
                    .is_some_and(|s| descends(&self.all, p.ppid, s.pid))
        })
    }

    fn watchers(&self, doorbell_only: bool) -> impl Iterator<Item = &Proc> {
        self.all.iter().filter(move |p| {
            p.has_args(&["inbox", "wait", "--stream"])
                && p.argv.iter().any(|a| a == "--doorbell-only") == doorbell_only
        })
    }

    fn describe(&self) -> String {
        self.all
            .iter()
            .map(|p| format!("{} <- {} {:?}", p.pid, p.ppid, p.argv))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The scratch deck, its refusing proxy and its project directory.
struct Real {
    proxy: RefusingProxy,
    work: PathBuf,
    events: common::EventSub,
    deck: TuiDeck,
}

/// The checks that keep these tests away from any model (see the module
/// documentation), run on every pane a test starts and at its end.
struct NoModel;

impl NoModel {
    /// The HOME holds no OpenCode credential and only this test's config.
    fn home(real: &Real) {
        let home = real.deck.home_dir();
        let data_roots = std::fs::read_dir(home)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("data-"))
            })
            .chain([home.join(".local/share")]);
        for auth in data_roots
            .map(|root| root.join("opencode/auth.json"))
            .chain([home.join(".config/opencode/auth.json")])
        {
            assert!(
                !auth.exists(),
                "no OpenCode credential may exist in the scratch HOME: {}",
                auth.display()
            );
        }
        let config: Value = serde_json::from_slice(
            &std::fs::read(home.join(".config/opencode/opencode.json")).expect("opencode.json"),
        )
        .expect("opencode.json is JSON");
        assert_eq!(
            config["enabled_providers"],
            json!([]),
            "OpenCode must have no provider enabled: {config}"
        );
        assert!(
            config.get("provider").is_none() && config.get("model").is_none(),
            "no provider or model may be configured: {config}"
        );
        assert!(
            !real.work.join("opencode.json").exists() && !real.work.join(".opencode").exists(),
            "no project config may add a provider"
        );
    }

    /// No OpenCode process of `tree` carries a credential-shaped variable,
    /// and each routes HTTP(S) to the refusing proxy.
    fn env(real: &Real, tree: &Tree) {
        let shaped = |name: &str| {
            let upper = name.to_ascii_uppercase();
            [
                "API_KEY",
                "ACCESS_KEY",
                "SECRET",
                "TOKEN",
                "PASSWORD",
                "CREDENTIAL",
            ]
            .iter()
            .any(|w| upper.contains(w))
        };
        for p in tree.opencode() {
            // OpenCode 2.x's client starts its private server with a password
            // it makes up for that server's own loopback API (measured on
            // 2.2.5: present on `serve --stdio`, absent from the client). It
            // opens nothing but that server, so it is allowed there and only
            // there, and only when the client did not pass on one of its own.
            let own_server_password = |k: &str| {
                k == "OPENCODE_PASSWORD"
                    && tree.server.as_ref().is_some_and(|s| s.pid == p.pid)
                    && !tree.client.env.contains_key(k)
            };
            let names: Vec<&String> = p
                .env
                .keys()
                .filter(|k| shaped(k) && !own_server_password(k))
                .collect();
            assert!(
                names.is_empty(),
                "OpenCode process {} ({:?}) must carry no credential-shaped variable, found \
                 {names:?}",
                p.pid,
                p.argv
            );
            for var in [
                "HTTPS_PROXY",
                "HTTP_PROXY",
                "ALL_PROXY",
                "https_proxy",
                "http_proxy",
                "all_proxy",
            ] {
                assert_eq!(
                    p.env.get(var),
                    Some(&real.proxy.url()),
                    "OpenCode process {} must route {var} to the refusing proxy",
                    p.pid
                );
            }
        }
    }

    /// The pane shows OpenCode's own "no provider" state.
    fn screen(real: &Real, pane: &Pane) {
        assert!(
            common::wait_for_pane_text_on(
                real.deck.attach_socket_path(),
                &pane.agent,
                "No provider selected",
                budget()
            ),
            "the OpenCode {}.x pane must draw `No provider selected`; pane:\n{}",
            pane.major,
            real.pane_text(pane)
        );
    }

    /// At the end: OpenCode sent at least one request through the refusing
    /// proxy, which shows it reads the proxy variables. This observes the
    /// requests that took the proxy, not every connection a process opens;
    /// what keeps a model out of reach is that none is configured ([`NoModel::home`],
    /// [`NoModel::screen`]) and no credential exists ([`NoModel::env`]).
    fn proxy(real: &Real) {
        let seen = real.proxy.seen();
        assert!(
            !seen.is_empty(),
            "the refusing proxy saw no connection, so nothing shows OpenCode honoured its proxy \
             variables (both pinned versions fetch their model catalog through it as they \
             start); if a newer OpenCode stopped doing that, this control needs another request \
             to watch"
        );
        eprintln!("refused by the proxy: {seen:?}");
        Self::home(real);
    }
}

impl Real {
    fn new() -> Self {
        let proxy = RefusingProxy::start();
        let deck = TuiDeck::builder()
            .with_pty_size(200, 50)
            .without_agent_credentials()
            .launch_with_fixture("minimal");
        assert!(
            common::wait_until(common::child_boot_budget(), || deck
                .attach_socket_path()
                .exists()),
            "the deck's daemon must listen"
        );
        let config = deck.home_dir().join(".config/opencode");
        std::fs::create_dir_all(&config).expect("create the OpenCode config root");
        std::fs::write(
            config.join("opencode.json"),
            r#"{"$schema":"https://opencode.ai/config.json","enabled_providers":[],"autoupdate":false,"share":"disabled"}"#,
        )
        .expect("write the no-provider config");
        let work = deck.workdir().join("project");
        std::fs::create_dir_all(&work).expect("create the project directory");
        let events = deck.subscribe_events();
        let real = Self {
            proxy,
            work,
            events,
            deck,
        };
        NoModel::home(&real);
        real
    }

    /// Start `bin args` as a declared OpenCode pane named `id`, as the TUI
    /// and desktop start an agent, with the scratch roots and the refusing
    /// proxy; returns once OpenCode runs and draws its no-provider state.
    fn start(&self, bin: &Path, args: &str, id: &str, major: u32) -> Pane {
        let data = self.deck.home_dir().join(".local/share");
        self.start_with_data(bin, args, id, major, data)
    }

    /// [`Real::start`] with OpenCode's data root (`XDG_DATA_HOME`) at `data`.
    fn start_with_data(&self, bin: &Path, args: &str, id: &str, major: u32, data: PathBuf) -> Pane {
        let home = self.deck.home_dir();
        let proxy = self.proxy.url();
        let loopback = "127.0.0.1,localhost,::1";
        let env = json!([
            ["XDG_CONFIG_HOME", home.join(".config")],
            ["XDG_DATA_HOME", data],
            ["XDG_STATE_HOME", home.join(".local/state")],
            ["XDG_CACHE_HOME", home.join(".cache")],
            ["HTTPS_PROXY", proxy],
            ["HTTP_PROXY", proxy],
            ["ALL_PROXY", proxy],
            ["https_proxy", proxy],
            ["http_proxy", proxy],
            ["all_proxy", proxy],
            ["NO_PROXY", loopback],
            ["no_proxy", loopback],
            // OpenCode 1.x npm-installs its plugin SDK into the config root as
            // it starts; offline, that fails at once instead of trying the
            // registry (`with_imported_opencode_credentials` does the same).
            ["npm_config_offline", "true"],
            ["DOT_AGENT_DECK_PANE_ID", id]
        ]);
        let command = format!("{} {args}", bin.display());
        let reply = inbox::attach(
            &self.deck,
            json!({"op":"start-agent","command":command.trim(),"cwd":self.work,
                "agent_type":"open_code","rows":40,"cols":140,"env":env,"display_name":id}),
        );
        assert_eq!(reply["ok"], true, "start OpenCode {major}.x: {reply}");
        let pane = Pane {
            agent: reply["id"].as_str().expect("an agent id").to_string(),
            id: id.to_string(),
            major,
            data,
        };
        NoModel::env(self, &self.tree(&pane));
        NoModel::screen(self, &pane);
        pane
    }

    fn stop(&self, pane: &Pane) {
        let reply = inbox::attach(&self.deck, json!({"op":"stop-agent","id":pane.agent}));
        assert_eq!(reply["ok"], true, "stop {}: {reply}", pane.id);
        assert!(
            common::wait_until(budget(), || !procs(self.deck.home_dir()).iter().any(|p| {
                p.env.get("DOT_AGENT_DECK_PANE_ID") == Some(&pane.id) && p.is_opencode()
            })),
            "{}'s OpenCode must exit once the pane is stopped",
            pane.id
        );
    }

    /// The pane's processes, once its OpenCode client (and, on 2.x, its
    /// private server) are running.
    fn tree(&self, pane: &Pane) -> Tree {
        let last = RefCell::new(Vec::new());
        let found = common::wait_until(budget(), || {
            *last.borrow_mut() = procs(self.deck.home_dir());
            Self::pick(&last.borrow(), pane).is_some()
        });
        let last = last.into_inner();
        assert!(
            found,
            "OpenCode {}.x must start in pane {}; processes:\n{:#?}",
            pane.major, pane.id, last
        );
        Self::pick(&last, pane).unwrap()
    }

    fn pick(all: &[Proc], pane: &Pane) -> Option<Tree> {
        let ours: Vec<&Proc> = all
            .iter()
            .filter(|p| p.env.get("DOT_AGENT_DECK_PANE_ID") == Some(&pane.id) && p.is_opencode())
            .collect();
        let client = ours
            .iter()
            .find(|p| !ours.iter().any(|q| q.pid == p.ppid))?;
        let server = ours
            .iter()
            .find(|p| p.ppid == client.pid && p.has_args(&["serve", "--stdio"]))
            .map(|p| (*p).clone());
        if pane.major >= 2 && server.is_none() {
            return None;
        }
        Some(Tree {
            all: all.to_vec(),
            client: (*client).clone(),
            server,
        })
    }

    /// Wait until both halves of the deck's plugin run their watchers in this
    /// pane's OpenCode ([`Tree::server_watcher`], [`Tree::tui_watcher`]) and
    /// the deck reports the pane's inbox armed by the plugin. Returns the
    /// tree it saw.
    fn plugin_loaded(&self, pane: &Pane) -> Tree {
        let last: RefCell<Option<Tree>> = RefCell::new(None);
        let loaded = common::wait_until(budget(), || {
            let tree = self.tree(pane);
            let ok = tree.server_watcher().is_some() && tree.tui_watcher().is_some();
            *last.borrow_mut() = Some(tree);
            ok
        });
        let tree = last.into_inner().expect("the pane's processes");
        assert!(
            loaded,
            "the deck's plugin must start both of its watchers inside OpenCode {}.x (the server \
             half's `--doorbell-only` one where server plugins run, the TUI half's in the \
             client); processes:\n{}\npane:\n{}",
            pane.major,
            tree.describe(),
            self.pane_text(pane)
        );
        let token = tree
            .client
            .env
            .get("DOT_AGENT_DECK_PANE_CAPABILITY")
            .expect("the deck gives the pane its capability")
            .clone();
        common::register_diagnostic_redactions([token.clone()]);
        let request = json!({"message_type":"inbox_list","pane_id":pane.id,
            "agent_id":pane.agent,"token":token,"all":false});
        let listing = RefCell::new(Value::Null);
        let armed = common::wait_until(budget(), || {
            let reply = inbox::hook(&self.deck, request.clone());
            let ok = reply["armed"].as_u64().unwrap_or(0) >= 1
                && reply["component"] == "opencode_plugin";
            *listing.borrow_mut() = reply;
            ok
        });
        let listing = listing.into_inner();
        assert!(
            armed,
            "the deck must report OpenCode {}.x's inbox armed by its plugin: {}",
            pane.major,
            json!({"armed":listing["armed"],"wake":listing["wake"],"component":listing["component"]})
        );
        tree
    }

    /// OpenCode's own reports of a plugin that failed to load: the log lines
    /// saying so (`failed to load plugin`, 2.x), and whether the pane shows
    /// the notice (`Plugin failed`, 2.x).
    fn failed_plugins(&self, pane: &Pane) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        if let Ok(entries) = std::fs::read_dir(pane.data.join(LOG_DIR)) {
            for entry in entries.flatten() {
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                lines.extend(
                    text.lines()
                        .filter(|l| l.to_ascii_lowercase().contains("failed to load plugin"))
                        .map(str::to_string),
                );
            }
        }
        let shown = common::pane_search_key_on(self.deck.attach_socket_path(), &pane.agent)
            .to_ascii_lowercase()
            .contains("pluginfailed");
        (lines, shown)
    }

    fn no_failed_plugin(&self, pane: &Pane) {
        let (lines, shown) = self.failed_plugins(pane);
        assert!(
            lines.is_empty() && !shown,
            "OpenCode {}.x must report no plugin that failed to load; log: {lines:#?}; on \
             screen: {shown}; pane:\n{}",
            pane.major,
            self.pane_text(pane)
        );
    }

    fn pane_text(&self, pane: &Pane) -> String {
        let key = common::pane_search_key_on(self.deck.attach_socket_path(), &pane.agent);
        key[key.len().saturating_sub(1500)..].to_string()
    }

    /// The `SessionStart` OpenCode's plugin reports for a session `session`
    /// accepts, owned by `pane`'s agent (the daemon attests the owner from the
    /// capability only the pane's environment carries). The `ses_` prefix
    /// keeps out the start the daemon itself surfaces for a new pane.
    fn session_start(&self, pane: &Pane, session: impl Fn(&str) -> bool) -> AgentEvent {
        self.events.wait_for(
            |e| {
                e.event_type == EventType::SessionStart
                    && e.agent_id.as_deref() == Some(pane.agent.as_str())
                    && e.session_id.starts_with("ses_")
                    && session(&e.session_id)
            },
            budget(),
        )
    }

    /// The status `daemon status --json` gives the agent's card, `None`
    /// while it has none (measured on both pinned versions: a new OpenCode
    /// pane's card has no status until a session of its OpenCode starts).
    fn card_status(&self, pane: &Pane) -> Option<String> {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"))
            .args(["daemon", "status", "--json"])
            .env_clear()
            .env(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                self.deck.attach_socket_path(),
            )
            .env("DOT_AGENT_DECK_SOCKET", self.deck.hook_socket_path())
            .env("HOME", self.deck.home_dir())
            .current_dir(self.deck.workdir())
            .output()
            .expect("run daemon status --json");
        assert!(
            output.status.success(),
            "daemon status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: Value = serde_json::from_slice(&output.stdout).expect("daemon status JSON");
        let record = document["agents"]
            .as_array()
            .expect("agents array")
            .iter()
            .find(|r| r["agent_id"] == pane.agent.as_str())
            .unwrap_or_else(|| panic!("no agent {} in daemon status: {document}", pane.agent))
            .clone();
        record["status"].as_str().map(str::to_string)
    }

    /// The pane's card reads `status`, in the daemon's record and on the
    /// rendered dashboard (`● <status>` in the card's title row).
    fn card_reads(&self, pane: &Pane, status: &str) {
        assert!(
            common::wait_until(budget(), || self.card_status(pane).as_deref()
                == Some(status)),
            "OpenCode {}.x's card must read {status}; it reads {:?}",
            pane.major,
            self.card_status(pane)
        );
        let shown = format!("● {status}");
        assert!(
            self.deck.wait_for_grid_string_within(&shown, budget()),
            "the dashboard must show {shown:?} on the card:\n{}",
            self.deck.snapshot_grid()
        );
    }
}

/// A free loopback port for OpenCode 1.x's local server.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("a free loopback port")
        .port()
}

/// `POST /session` on OpenCode 1.x's loopback server, which creates a session
/// and needs no model. Returns its id.
fn create_session_v1(port: u16, directory: &Path) -> String {
    let path = format!(
        "/session?directory={}",
        encode(&directory.to_string_lossy())
    );
    let last = RefCell::new(String::new());
    let id = RefCell::new(None);
    let ok = common::wait_until(budget(), || {
        let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
             Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        let body = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        let found = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v["id"].as_str().map(str::to_string));
        let ok = found.is_some();
        *id.borrow_mut() = found;
        *last.borrow_mut() = response;
        ok
    });
    assert!(
        ok,
        "OpenCode 1.x's server must create a session: {}",
        last.borrow()
    );
    id.into_inner().unwrap()
}

/// Percent-encode a query value (RFC 3986 unreserved characters and `/` kept).
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Scenario: Start the pinned real OpenCode 1.x in a deck pane by its path,
/// with no provider, no credential and a proxy that answers nothing; the pane
/// draws OpenCode's own `No provider selected`. Both halves of the deck's
/// plugin must start their watchers inside it and the deck must report the
/// pane armed by the plugin; a session created through OpenCode's own
/// loopback server must reach the agent's card, owned by that pane, whose
/// status goes from none to Idle.
#[spec("hooks/opencode-real/001")]
#[test]
fn opencode_real_001_opencode_1_loads_the_plugin_and_a_session_reaches_the_card() {
    let bin = pinned_or_skip!(1);
    let real = Real::new();
    let port = free_port();
    let pane = real.start(&bin, &format!("--port {port}"), "real-opencode-1", 1);
    let tree = real.plugin_loaded(&pane);
    assert!(
        !tree.client.argv.iter().any(|a| a == "--standalone"),
        "the deck adds no `--standalone` to OpenCode 1.x: {:?}",
        tree.client.argv
    );
    assert_eq!(
        real.card_status(&pane),
        None,
        "before any session, the card has no status"
    );
    let session = create_session_v1(port, &real.work);
    let start = real.session_start(&pane, |id| id == session);
    assert_eq!(start.pane_id.as_deref(), Some(pane.id.as_str()));
    real.card_reads(&pane, "Idle");
    real.no_failed_plugin(&pane);
    NoModel::env(&real, &real.tree(&pane));
    NoModel::proxy(&real);
}

/// Scenario: Start the pinned real OpenCode 2.x in a deck pane by its path,
/// with no provider, no credential and a proxy that answers nothing. The deck
/// must start it with `--standalone`, so its private server runs with the
/// pane's environment: the server half of the plugin starts its watcher in
/// that server, the TUI half in the client, and the deck reports the pane
/// armed. A deck message sent to the pane must start a session owned by the
/// pane; with no model its turn fails, so the card's status goes from none to
/// Error, and OpenCode reports no plugin that failed to load.
#[spec("hooks/opencode-real/002")]
#[test]
fn opencode_real_002_opencode_2_gets_standalone_and_its_plugin_runs_with_the_pane_env() {
    let bin = pinned_or_skip!(2);
    let real = Real::new();
    let pane = real.start(&bin, "", "real-opencode-2", 2);
    let tree = real.plugin_loaded(&pane);
    assert_eq!(
        tree.client
            .argv
            .iter()
            .filter(|a| *a == "--standalone")
            .count(),
        1,
        "the deck starts OpenCode 2.x with `--standalone` once: {:?}",
        tree.client.argv
    );
    let server = tree.server.as_ref().expect("2.x's private server");
    for (var, want) in [
        ("DOT_AGENT_DECK_PANE_ID", pane.id.as_str()),
        ("DOT_AGENT_DECK_AGENT_ID", pane.agent.as_str()),
    ] {
        assert_eq!(
            server.env.get(var).map(String::as_str),
            Some(want),
            "2.x's private server, which hosts the server plugin, has the pane's {var}"
        );
    }
    assert_eq!(
        real.card_status(&pane),
        None,
        "before any session, the card has no status"
    );
    let posted = inbox::attach(
        &real.deck,
        json!({"op":"post-message","pane_id":pane.id,"expected_agent_id":pane.agent,
            "subject":"real OpenCode 2.x","body":"OPENCODE-REAL-2051: there is no model here.",
            "origin":"seed"}),
    );
    assert_eq!(posted["ok"], true, "post a deck message: {posted}");
    let start = real.session_start(&pane, |_| true);
    assert_eq!(start.pane_id.as_deref(), Some(pane.id.as_str()));
    real.events.wait_for(
        |e| {
            e.event_type == EventType::Error
                && e.session_id == start.session_id
                && e.agent_id.as_deref() == Some(pane.agent.as_str())
        },
        budget(),
    );
    real.card_reads(&pane, "Error");
    real.no_failed_plugin(&pane);
    NoModel::env(&real, &real.tree(&pane));
    NoModel::proxy(&real);
}

/// Scenario: As `hooks/opencode-real/002`, with a plugin that throws as it
/// loads put beside the deck's. Real OpenCode 2.x must report that one plugin
/// as failed, in its log and on screen, naming it and not the deck's files,
/// while the deck's plugin still loads: so the check 002 and 004 rely on can
/// see a failure.
#[spec("hooks/opencode-real/003")]
#[test]
fn opencode_real_003_opencode_2_reports_a_broken_plugin_beside_the_decks() {
    let bin = pinned_or_skip!(2);
    let real = Real::new();
    let broken = real
        .deck
        .home_dir()
        .join(".config/opencode/plugin/broken-control-2051.js");
    std::fs::create_dir_all(broken.parent().unwrap()).expect("create the plugin directory");
    std::fs::write(&broken, "throw new Error('broken-control-2051');\n")
        .expect("write the broken plugin");
    let pane = real.start(&bin, "", "real-opencode-broken", 2);
    real.plugin_loaded(&pane);
    let seen = RefCell::new((Vec::new(), false));
    let reported = common::wait_until(budget(), || {
        let now = real.failed_plugins(&pane);
        let ok = !now.0.is_empty() && now.1;
        *seen.borrow_mut() = now;
        ok
    });
    let seen = seen.into_inner();
    assert!(
        reported,
        "OpenCode 2.x must report the broken plugin in its log and on screen; log: {:#?}; on \
         screen: {}; pane:\n{}",
        seen.0,
        seen.1,
        real.pane_text(&pane)
    );
    for line in &seen.0 {
        assert!(
            line.contains("broken-control-2051.js") && !line.contains("dot-agent-deck"),
            "only the broken plugin fails: {line}"
        );
    }
    NoModel::proxy(&real);
}

/// Scenario: With one OpenCode config root, start the pinned real OpenCode
/// 2.x, stop it, start the pinned 1.x, stop it, and start 2.x again, each in
/// its own deck pane with no model and its own data root. Each start must
/// load the deck's plugin and report no failed plugin; the first 2.x start
/// copies the deck's 1.x TUI entry into a new `cli.json`, the 1.x start's
/// install takes it out again and the second 2.x start finds none, and the
/// deck's server module is never rewritten.
#[spec("hooks/opencode-real/004")]
#[test]
fn opencode_real_004_switching_between_opencode_1_and_2_keeps_the_plugin_loading() {
    let v1 = pinned_or_skip!(1);
    let v2 = pinned_or_skip!(2);
    let real = Real::new();
    let home = real.deck.home_dir().to_path_buf();
    let cli_json = home.join(".config/opencode/cli.json");
    let cli_text = || std::fs::read_to_string(&cli_json).unwrap_or_default();
    let module = || {
        let path = home.join(SERVER_MODULE);
        let meta = std::fs::metadata(&path).expect("the deck's server module");
        (
            std::fs::read(&path).expect("read the server module"),
            std::os::unix::fs::MetadataExt::ino(&meta),
            meta.modified().expect("mtime"),
        )
    };

    // One config root, where the deck's plugin lives, and a data root per
    // version: OpenCode 2.x migrates its database in place, after which 1.19.5
    // refuses to start (`Database is not empty and has no session table`,
    // measured), whatever the deck does. Each 2.x start gets a fresh one, so
    // each reads its own log.
    let data = |name: &str| real.deck.home_dir().join(name);
    let first = real.start_with_data(&v2, "", "real-opencode-switch-a", 2, data("data-2a"));
    real.plugin_loaded(&first);
    real.no_failed_plugin(&first);
    let installed = module();
    real.stop(&first);
    // The thing to repair: 2.x copied the deck's 1.x TUI entry out of
    // `tui.json` into the `cli.json` it created (measured on 2.2.5). If a
    // newer 2.x stops doing that, the repair below has nothing to act on and
    // this precondition, not a silent pass, says so.
    assert!(
        cli_text().contains("dot-agent-deck"),
        "precondition: the first 2.x start leaves a deck entry in cli.json for the next \
         install to remove; cli.json: {:?}",
        cli_text()
    );

    let second = real.start_with_data(&v1, "", "real-opencode-switch-b", 1, data("data-1"));
    real.plugin_loaded(&second);
    real.no_failed_plugin(&second);
    assert!(
        !cli_text().contains("dot-agent-deck"),
        "after the 1.x start's install, cli.json holds no deck entry: {}",
        cli_text()
    );
    real.stop(&second);

    let third = real.start_with_data(&v2, "", "real-opencode-switch-c", 2, data("data-2b"));
    real.plugin_loaded(&third);
    real.no_failed_plugin(&third);
    assert!(
        !cli_text().contains("dot-agent-deck"),
        "2.x started again finds no deck entry in cli.json: {}",
        cli_text()
    );
    assert!(
        module() == installed,
        "the deck's server module keeps its bytes, inode and mtime across the switch (OpenCode \
         2 reloads its plugins when the file changes)"
    );
    NoModel::proxy(&real);
}
