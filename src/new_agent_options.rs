//! PRD #1223 M2: what the daemon tells a new-agent form about the deck it will
//! start the agent on — what [`crate::daemon_protocol::AttachRequest::NewAgentOptions`]
//! answers.
//!
//! The desktop's form needs four facts the TUI reads from its own process, and
//! on a remote deck the desktop's process is the wrong place to read any of
//! them: the configured default command lives in a file on the deck's host, the
//! agent registry is whichever one the deck's build compiled in, and the
//! experimental flag has meaning where the spawn happens. So the daemon answers
//! for itself rather than the desktop computing an answer from its own build.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::DashboardConfig;

/// PRD #1223 audit A4: how many new-agent form queries —
/// [`crate::daemon_protocol::AttachRequest::ListDirectories`] and
/// [`crate::daemon_protocol::AttachRequest::NewAgentOptions`] together — may
/// hold a blocking thread across the whole daemon at once.
///
/// **4**, in a pool of their OWN. They used to wait on the project verbs'
/// [`crate::project_resolve::MAX_CONCURRENT_PROJECT_READS`] permits (the
/// listing) or take no permit at all (the options query), so a burst of
/// listings against a slow directory could hold every permit `ResolveProject`
/// and `PrepareOrchestration` need, and a burst of options queries could spawn one
/// blocking job each. With their own pool neither can occupy a project-verb
/// permit, and together they occupy at most this many blocking threads.
///
/// 4 rather than fewer because one dialog opening already sends two at once (the
/// options query and the home listing), and a person clicking through a slow
/// directory can leave a superseded listing still in flight; 2 would refuse
/// ordinary use of one form, where 4 leaves room for a second client.
///
/// A query that finds the pool full is **refused, not queued**
/// ([`run_new_agent_query`]): a queued query would still be a connection held
/// open behind a slow filesystem, and the refusal is the retryable
/// [`crate::daemon_protocol::PROJECT_ERR_BUSY`].
pub const MAX_CONCURRENT_NEW_AGENT_QUERIES: usize = 4;

/// Why [`run_new_agent_query`] did not run its closure to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewAgentQueryError {
    /// Every one of the [`MAX_CONCURRENT_NEW_AGENT_QUERIES`] permits was held,
    /// so nothing was spawned. Retryable.
    Busy,
    /// The blocking task did not complete (it panicked or was cancelled).
    Failed,
}

fn new_agent_query_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_NEW_AGENT_QUERIES)))
}

/// Run one new-agent form query on a blocking thread, behind the daemon-wide
/// [`MAX_CONCURRENT_NEW_AGENT_QUERIES`] bound — or refuse it as
/// [`NewAgentQueryError::Busy`] without spawning anything when the bound is
/// reached.
///
/// The permit is taken with `try_acquire`, never awaited, and it is moved into
/// the blocking closure's own frame, so it is held for exactly as long as a
/// thread is occupied and an unwinding panic releases it too — the arrangement
/// [`crate::project_resolve::run_bounded`] uses, minus the queue.
pub async fn run_new_agent_query<T, F>(f: F) -> Result<T, NewAgentQueryError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    try_run_bounded(new_agent_query_limit(), f).await
}

/// [`run_new_agent_query`] against a given pool, so the refuse-not-queue rule is
/// testable without saturating the daemon-wide one.
async fn try_run_bounded<T, F>(limit: &Arc<Semaphore>, f: F) -> Result<T, NewAgentQueryError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = limit
        .clone()
        .try_acquire_owned()
        .map_err(|_| NewAgentQueryError::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|_| NewAgentQueryError::Failed)
}

/// Test-only: hold every permit of the daemon-wide pool, as a burst of slow
/// queries would, so a test can observe what the dispatch does when it is full.
///
/// Tests that do this, and tests that send either query through the real
/// dispatch, serialise on [`POOL_TEST_GUARD`] so a `cargo test` run (which,
/// unlike nextest, shares one process across tests) cannot hand one of them a
/// `busy` it did not cause.
#[cfg(test)]
pub(crate) async fn saturate_new_agent_query_pool() -> tokio::sync::OwnedSemaphorePermit {
    new_agent_query_limit()
        .clone()
        .acquire_many_owned(MAX_CONCURRENT_NEW_AGENT_QUERIES as u32)
        .await
        .expect("the new-agent query pool is never closed")
}

/// See [`saturate_new_agent_query_pool`].
#[cfg(test)]
pub(crate) static POOL_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// PRD #1223 M2: the daemon's reply to
/// [`crate::daemon_protocol::AttachRequest::NewAgentOptions`], carried on
/// [`crate::daemon_protocol::AttachResponse::new_agent_options`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgentOptions {
    /// `DashboardConfig.default_command` from the configuration file on the
    /// daemon's host — the file the TUI reads there, `DOT_AGENT_DECK_CONFIG`
    /// included. Absent when that value is empty, which is the unconfigured
    /// default; otherwise verbatim, as the TUI's own prefill uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_command: Option<String>,
    /// `DashboardConfig.default_dir` from the same file — the directory a
    /// client's browser opens in instead of the daemon user's home (PRD #1223).
    ///
    /// **Validated, canonicalised, and absent rather than an error** when it is
    /// anything but a readable directory ([`usable_default_dir`]): unset,
    /// relative, a control character in it, missing, a file, unreadable, or a
    /// canonical form that is not UTF-8. A bad setting must never fail the
    /// options query — the form then has no default command either, which is a
    /// worse outcome than browsing from home. Canonical because the value goes
    /// straight back to [`crate::daemon_protocol::AttachRequest::ListDirectories`],
    /// whose own boundary check it therefore already passes.
    ///
    /// **An additive optional field on a reply this PRD introduced**, already
    /// gated behind [`crate::daemon_protocol::CAP_NEW_AGENT_OPTIONS`]: an older
    /// client ignores it (the struct sets no `deny_unknown_fields`), and a
    /// daemon predating it simply omits it, which a newer client reads as
    /// "open in home". No capability of its own and no `PROTOCOL_VERSION` move
    /// — the protocol's written "Do NOT bump" case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_dir: Option<String>,
    /// The agent registry this daemon was built with
    /// ([`crate::agent_registry::ALL`]), in registry order.
    #[serde(default)]
    pub agents: Vec<AgentOption>,
    /// The daemon process's own experimental-flag state
    /// ([`crate::features::experimental_enabled`]). A client uses it to decide
    /// which surfaces to show for this deck; the daemon branches on nothing
    /// here.
    #[serde(default)]
    pub experimental: bool,
    /// The authoring kinds this daemon can compose a seed for — the values
    /// `StartAgent.authoring_kind` accepts ([`crate::authoring_seeds::AuthoringKind::ALL`],
    /// in the TUI Mode cycler's order). Every kind this build knows, whatever
    /// the experimental flag says: the flag decides what a client SHOWS, which
    /// is the client's call (the desktop hides `schedule-issues` unless
    /// [`Self::experimental`] is true, as the TUI hides its chip), not what the
    /// daemon can do. Empty from a daemon predating PRD #1223 M7, which is the
    /// honest answer from one that cannot.
    #[serde(default)]
    pub authoring_kinds: Vec<String>,
}

/// One agent in [`NewAgentOptions::agents`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOption {
    /// The registry entry's first `detect_basenames` value (`claude`,
    /// `opencode`, …) — a stable key, where the display name is prose.
    pub id: String,
    /// `AgentSpec.label`.
    pub display_name: String,
    /// `AgentSpec.default_command`: the command the form's Agent picker writes
    /// into Command when this agent is chosen.
    #[serde(default)]
    pub default_command: Option<String>,
}

/// The options this daemon process reports, read at the moment of the request.
///
/// **Blocking** — it reads the host's `config.toml` — so the dispatch runs it
/// through [`run_new_agent_query`]. Read per request rather than cached:
/// `DashboardConfig` has no reload path in the daemon, and the TUI re-reads the
/// same file whenever it starts, so a cache here would be the one reader that
/// could go stale. Read with [`DashboardConfig::load_bounded`], so a FIFO or an
/// oversized file at the config path cannot hold the thread or its memory.
pub fn for_this_daemon() -> NewAgentOptions {
    compose(
        &DashboardConfig::load_bounded(),
        crate::features::experimental_enabled(),
    )
}

/// [`for_this_daemon`]'s projection, with its two inputs passed in so it is
/// testable without a config file or the process-global flag. It touches the
/// filesystem only to vet [`DashboardConfig::default_dir`].
pub fn compose(config: &DashboardConfig, experimental: bool) -> NewAgentOptions {
    NewAgentOptions {
        default_command: Some(config.default_command.clone()).filter(|c| !c.is_empty()),
        default_dir: usable_default_dir(&config.default_dir),
        agents: registry_agents(),
        experimental,
        authoring_kinds: crate::authoring_seeds::AuthoringKind::ALL
            .iter()
            .map(|kind| kind.as_str().to_string())
            .collect(),
    }
}

/// The configured default directory, if it is one a listing would accept —
/// or `None`, never an error (see [`NewAgentOptions::default_dir`]). The TUI's
/// directory picker calls it too, on its own `DashboardConfig`, so both clients
/// vet the setting identically.
///
/// The same three gates a caller-supplied listing path passes, in the same
/// order: the wire-boundary predicate
/// ([`crate::agent_pty::is_valid_orchestration_cwd`] — absolute, bounded, no
/// control characters) before any filesystem access, then the project reader's
/// canonicaliser (a directory, UTF-8), then the canonical form through the
/// predicate again, because resolving a symlink can lengthen a path. Finally
/// the directory has to OPEN, so a `0o000` directory is reported absent rather
/// than handed to a browser whose first listing would fail. Opening a
/// directory reads nothing, so this cannot block on a FIFO: the canonicaliser
/// has already refused anything that is not a directory.
pub fn usable_default_dir(raw: &str) -> Option<String> {
    if raw.is_empty() || !crate::agent_pty::is_valid_orchestration_cwd(raw) {
        return None;
    }
    let canonical =
        crate::project_resolve::canonicalize_project_dir(std::path::Path::new(raw)).ok()?;
    let text = canonical.to_str()?.to_string();
    if !crate::agent_pty::is_valid_orchestration_cwd(&text) {
        return None;
    }
    std::fs::read_dir(&canonical).ok()?;
    Some(text)
}

/// [`crate::agent_registry::ALL`] projected onto the wire, in registry order.
///
/// An entry with no `detect_basenames` has no stable id and is left out rather
/// than sent with an invented one. No shipped entry is in that position — the
/// neutral `NONE` placeholder has none, and it is not in `ALL`.
pub fn registry_agents() -> Vec<AgentOption> {
    crate::agent_registry::ALL
        .iter()
        .filter_map(|spec| {
            Some(AgentOption {
                id: (*spec.detect_basenames.first()?).to_string(),
                display_name: spec.label.to_string(),
                default_command: spec.default_command.map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_dir(default_dir: &str) -> DashboardConfig {
        DashboardConfig {
            default_dir: default_dir.to_string(),
            ..DashboardConfig::default()
        }
    }

    /// Scenario (PRD #1223): a deck whose config names an existing directory
    /// reports it, canonicalised, beside its default command.
    #[test]
    fn a_set_default_dir_is_served_canonicalised() {
        let root = tempfile::tempdir().unwrap();
        let reports = root.path().join("reports");
        std::fs::create_dir(&reports).unwrap();
        let canonical = std::fs::canonicalize(&reports).unwrap();
        let options = compose(&config_with_dir(reports.to_str().unwrap()), false);
        assert_eq!(options.default_dir.as_deref(), canonical.to_str());
        // A spelling with `..` in it names the same directory and is reported
        // in its canonical form, which is what a listing request would accept.
        let dotted = root.path().join("reports").join("..").join("reports");
        assert_eq!(
            compose(&config_with_dir(dotted.to_str().unwrap()), false).default_dir,
            options.default_dir
        );
    }

    /// Scenario (PRD #1223): unset, relative, missing, a file, or a control
    /// character — each is reported ABSENT, and the rest of the reply is still
    /// served, so a bad setting never breaks the options query.
    #[test]
    fn an_unusable_default_dir_is_absent_and_never_fails_the_query() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let missing = root.path().join("gone");
        let control = format!("{}/bad\u{1b}[31m", root.path().display());
        for raw in [
            "",
            "relative/path",
            "./x",
            missing.to_str().unwrap(),
            file.to_str().unwrap(),
            control.as_str(),
        ] {
            let mut config = config_with_dir(raw);
            config.default_command = "claude".to_string();
            let options = compose(&config, false);
            assert_eq!(options.default_dir, None, "{raw:?}");
            assert_eq!(
                options.default_command.as_deref(),
                Some("claude"),
                "{raw:?}"
            );
            assert!(!options.agents.is_empty(), "{raw:?}");
        }
    }

    /// Scenario (PRD #1223): a directory the daemon user cannot open is
    /// absent — the browser would otherwise open on a listing that fails.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_default_dir_is_absent() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can open anything, so the property is only observable as a
        // user the mode actually binds.
        let binds = std::fs::read_dir(&locked).is_err();
        let served = usable_default_dir(locked.to_str().unwrap());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if binds {
            assert_eq!(served, None);
        }
    }

    /// The field is additive and optional on the wire: omitted when absent, and
    /// a reply from a daemon predating it deserialises with it `None`.
    #[test]
    fn the_wire_shape_omits_an_absent_default_dir_and_reads_one_from_an_older_daemon() {
        let json = serde_json::to_value(compose(&config_with(""), false)).unwrap();
        assert!(json.get("default_dir").is_none());
        let older: NewAgentOptions =
            serde_json::from_value(serde_json::json!({ "agents": [], "experimental": false }))
                .unwrap();
        assert_eq!(older.default_dir, None);
    }

    fn config_with(default_command: &str) -> DashboardConfig {
        DashboardConfig {
            default_command: default_command.to_string(),
            ..DashboardConfig::default()
        }
    }

    /// Audit A4: a full pool refuses the next query at once, without spawning
    /// it, rather than parking it until a permit frees — and a permit returns to
    /// the pool when the query holding it finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_pool_refuses_the_next_query_rather_than_queueing_it() {
        let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_NEW_AGENT_QUERIES));
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let mut releases = Vec::new();
        let mut running = Vec::new();
        for _ in 0..MAX_CONCURRENT_NEW_AGENT_QUERIES {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            releases.push(release_tx);
            let started_tx = started_tx.clone();
            let limit = limit.clone();
            running.push(tokio::spawn(async move {
                try_run_bounded(&limit, move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .await
            }));
        }
        for _ in 0..MAX_CONCURRENT_NEW_AGENT_QUERIES {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("every in-pool query reaches its blocking thread");
        }

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in_job = ran.clone();
        let excess = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            try_run_bounded(&limit, move || {
                ran_in_job.store(true, std::sync::atomic::Ordering::SeqCst)
            }),
        )
        .await
        .expect("a full pool answers at once — a query that waits here was queued");
        assert_eq!(excess, Err(NewAgentQueryError::Busy));
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "a refused query spawns nothing"
        );

        for release in releases {
            release.send(()).unwrap();
        }
        for query in running {
            assert_eq!(query.await.unwrap(), Ok(()));
        }
        assert_eq!(
            try_run_bounded(&limit, || 7).await,
            Ok(7),
            "the permits return to the pool when the queries holding them finish"
        );
    }

    /// Audit A4: with the daemon-wide new-agent pool saturated, the queries are
    /// refused while the project verbs' own pool still hands out a permit — the
    /// two are separate, so listings can no longer starve `ResolveProject` /
    /// `PrepareOrchestration`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_verbs_still_acquire_while_the_new_agent_pool_is_saturated() {
        let _serial = POOL_TEST_GUARD.lock().await;
        let held = saturate_new_agent_query_pool().await;

        assert_eq!(
            run_new_agent_query(|| ()).await,
            Err(NewAgentQueryError::Busy),
            "a saturated pool refuses the next query"
        );
        let project_work = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::project_resolve::run_bounded(|| 7),
        )
        .await
        .expect("a project verb acquires its own permit while the new-agent pool is full")
        .expect("the project work runs");
        assert_eq!(project_work, 7);

        drop(held);
        assert_eq!(run_new_agent_query(|| 1).await, Ok(1));
    }

    #[test]
    fn the_new_agent_query_bound_is_small_and_its_own() {
        assert_eq!(MAX_CONCURRENT_NEW_AGENT_QUERIES, 4);
    }

    #[test]
    fn the_registry_is_projected_in_order_with_its_first_basename_as_the_id() {
        let agents = registry_agents();
        assert_eq!(agents.len(), crate::agent_registry::ALL.len());
        for (agent, spec) in agents.iter().zip(crate::agent_registry::ALL) {
            assert_eq!(agent.id, spec.detect_basenames[0]);
            assert_eq!(agent.display_name, spec.label);
            assert_eq!(agent.default_command.as_deref(), spec.default_command);
        }
        assert_eq!(
            agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["claude", "opencode", "pi", "codex", "devin"]
        );
    }

    #[test]
    fn an_empty_default_command_is_absent_and_a_set_one_is_verbatim() {
        assert_eq!(compose(&config_with(""), false).default_command, None);
        assert_eq!(
            compose(&config_with("opencode --model x"), false)
                .default_command
                .as_deref(),
            Some("opencode --model x")
        );
    }

    #[test]
    fn the_flag_is_reported_as_given_and_every_authoring_kind_is_offered_either_way() {
        for experimental in [false, true] {
            let options = compose(&config_with("claude"), experimental);
            assert_eq!(options.experimental, experimental);
            assert_eq!(
                options.authoring_kinds,
                ["schedule", "schedule-issues", "dispatcher"],
                "PRD #1223 M7: the daemon lists every kind it can compose; filtering \
                 `schedule-issues` on the flag is the client's job"
            );
        }
    }

    #[test]
    fn the_wire_shape_lists_the_authoring_kinds_and_omits_an_absent_command() {
        let json = serde_json::to_value(compose(&config_with(""), false)).unwrap();
        assert!(json.get("default_command").is_none());
        assert_eq!(
            json["authoring_kinds"],
            serde_json::json!(["schedule", "schedule-issues", "dispatcher"])
        );
        assert_eq!(json["experimental"], serde_json::json!(false));
        assert_eq!(
            json["agents"][0],
            serde_json::json!({
                "id": "claude",
                "display_name": crate::agent_registry::CLAUDE_CODE.label,
                "default_command": "claude",
            })
        );
        let back: NewAgentOptions = serde_json::from_value(json).unwrap();
        assert_eq!(back, compose(&config_with(""), false));
    }
}
