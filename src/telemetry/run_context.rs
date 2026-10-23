//! Run context the deck derives from a pane's working directory: the commit
//! its instructions were last changed in, and the repository's name.
//!
//! Each is absent when it cannot be determined — outside a git repository, with
//! no history for the instruction paths, with no `origin` remote, or when a
//! probe does not finish in time — and never guessed. The working directory
//! itself is read here and never sent.
//!
//! **The probes switch off the ways configuration is known to make these reads
//! run other programs.** A repository's or the user's git configuration can
//! make an ordinary read run another program: a signature verifier for
//! `log.showSignature`, an fsmonitor hook, a pager, a lazy fetch of missing
//! objects in a partial clone (and the transport and credential helpers behind
//! it). [`harden`] switches each of those off for these two commands, and the
//! remote URL is read as configured (`git config`), without `insteadOf`
//! rewriting. The lazy fetch is switched off by `GIT_NO_LAZY_FETCH`, which Git
//! honours from 2.45; on an older Git, the remaining defence is
//! `protocol.allow=never` with a `protocol.<name>.allow=never` for each
//! transport Git ships ([`HARDENING_CONFIG`]), given on the command line, which
//! takes precedence over a repository's own `protocol.<name>.allow` for those
//! names. A repository can still allow a scheme outside that list, which runs a
//! `git-remote-<scheme>` helper only if one is installed.
//!
//! **This assumes a trusted `git`.** The probes run the `git` the daemon's
//! `PATH` resolves first, with the inherited global and system configuration;
//! whoever controls that `PATH` or the daemon's environment chooses the program
//! that runs. It is a hardening of trusted git against repository content, not
//! a sandbox.
//!
//! **And they are bounded.** Each probe has a deadline after which its process
//! is killed and reaped, its captured output is capped, at most
//! [`MAX_CONCURRENT_PROBES`] run at once, and the deck cancels outstanding ones
//! when the daemon shuts down ([`Probes::shutdown`]). Nothing here runs on the
//! spawn path or on the runtime's blocking pool, which a runtime waits for when
//! it stops.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::{Semaphore, watch};

/// The instruction files whose last commit is `deck.instructions.commit`, at
/// the root of the repository whatever directory in it the agent runs from.
pub const INSTRUCTION_PATHS: [&str; 3] =
    [":(top)CLAUDE.md", ":(top)AGENTS.md", ":(top).claude/skills"];

/// How long one git command may run before it is killed.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);
/// Git commands running at once, at most, across all runs.
pub const MAX_CONCURRENT_PROBES: usize = 4;
/// Runs whose context is waiting to be resolved, at most. Past it a new run's
/// context is left absent rather than queued.
pub const MAX_QUEUED_RUNS: usize = 64;
/// Bytes of a probe's output read, at most. Both answers are one short line.
pub const MAX_PROBE_OUTPUT: u64 = 4096;
/// How long a killed probe is waited for, to reap it.
const REAP_WAIT: Duration = Duration::from_secs(1);

/// `-c` overrides that keep a read-only git command from running anything
/// configuration names.
pub const HARDENING_CONFIG: [&str; 13] = [
    "log.showSignature=false",
    "core.fsmonitor=false",
    "credential.helper=",
    // No transport: defence in depth behind `GIT_NO_LAZY_FETCH`, which a Git
    // older than 2.45 ignores. `protocol.allow` is only the default, which a
    // repository's `protocol.<name>.allow` overrides for that name, so each
    // transport Git ships is named as well; a `-c` on the command line takes
    // precedence over the repository's configuration for the same key.
    "protocol.allow=never",
    "protocol.ext.allow=never",
    "protocol.file.allow=never",
    "protocol.ssh.allow=never",
    "protocol.git.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
    "protocol.ftp.allow=never",
    "protocol.ftps.allow=never",
    "core.untrackedCache=false",
];

/// Environment set for the probes, on top of [`HARDENING_CONFIG`].
pub const HARDENING_ENV: [(&str, &str); 3] = [
    ("GIT_NO_LAZY_FETCH", "1"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_OPTIONAL_LOCKS", "0"),
];

/// Environment removed for the probes: configuration injected through the
/// environment, programs named by it, and the switches that would read
/// [`INSTRUCTION_PATHS`] as something other than root-anchored pathspecs.
pub const HARDENING_ENV_REMOVED: [&str; 13] = [
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_PROXY_COMMAND",
];

/// What [`resolve_with`] found for a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunContext {
    pub instructions_commit: Option<String>,
    pub repository_name: Option<String>,
}

/// How probes are run. Tests replace the program with a stand-in.
#[derive(Debug, Clone)]
pub struct ProbeSettings {
    /// `None`: git, as [`crate::git_env::git_async`] builds it. `Some`: this
    /// program with these leading arguments, then git's arguments — a
    /// stand-in that hangs.
    #[cfg(test)]
    pub program: Option<(PathBuf, Vec<String>)>,
    pub deadline: Duration,
    pub max_concurrent: usize,
    pub max_queued: usize,
}

impl Default for ProbeSettings {
    fn default() -> Self {
        ProbeSettings {
            #[cfg(test)]
            program: None,
            deadline: PROBE_DEADLINE,
            max_concurrent: MAX_CONCURRENT_PROBES,
            max_queued: MAX_QUEUED_RUNS,
        }
    }
}

/// The deck's probe runner: the concurrency bound, the queue bound and the
/// shutdown switch shared by every run's probes.
pub struct Probes {
    settings: ProbeSettings,
    permits: Arc<Semaphore>,
    cancel: watch::Sender<bool>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
}

impl Probes {
    pub fn new(settings: ProbeSettings) -> Self {
        Probes {
            permits: Arc::new(Semaphore::new(settings.max_concurrent.max(1))),
            settings,
            cancel: watch::channel(false).0,
            tasks: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    /// Resolve `cwd`'s context on the current runtime and hand it to `done`.
    /// Nothing happens without a runtime, when the queue is full, or after
    /// [`Self::shutdown`]: the run's context stays absent.
    pub fn spawn(&self, cwd: PathBuf, done: impl FnOnce(RunContext) + Send + 'static) {
        if *self.cancel.borrow() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= self.settings.max_queued {
            tracing::debug!(
                "telemetry: run-context probes are backed up; leaving this run's absent"
            );
            return;
        }
        let settings = self.settings.clone();
        let permits = Arc::clone(&self.permits);
        let cancel = self.cancel.subscribe();
        tasks.spawn_on(
            async move {
                let context = resolve_with(&settings, &permits, cancel, &cwd).await;
                done(context);
            },
            &runtime,
        );
    }

    /// Daemon shutdown: cancel every outstanding probe, which kills and reaps
    /// its process, and wait for them up to `budget`; what is still running
    /// after it is aborted.
    pub async fn shutdown(&self, budget: Duration) {
        self.cancel.send_replace(true);
        let mut tasks = std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|p| p.into_inner()));
        let drained =
            tokio::time::timeout(budget, async { while tasks.join_next().await.is_some() {} })
                .await;
        if drained.is_err() {
            tasks.abort_all();
        }
    }

    /// Probes queued or running.
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.len()
    }
}

/// Run the two probes in `cwd` under `settings`, each holding one of `permits`
/// while it runs. Either is absent if it fails, times out or is cancelled.
pub async fn resolve_with(
    settings: &ProbeSettings,
    permits: &Semaphore,
    cancel: watch::Receiver<bool>,
    cwd: &Path,
) -> RunContext {
    // No check that `cwd` exists first: that is a blocking call, and git
    // answers a missing directory with a failure anyway.
    let mut log_args = vec!["log", "--no-show-signature", "-1", "--format=%H", "--"];
    log_args.extend(INSTRUCTION_PATHS);
    let instructions_commit = probe(settings, permits, cancel.clone(), cwd, &log_args)
        .await
        .filter(|hash| is_full_hash(hash));
    let repository_name = probe(
        settings,
        permits,
        cancel,
        cwd,
        &["config", "--get", "remote.origin.url"],
    )
    .await
    .as_deref()
    .and_then(repository_name_from_remote_url);
    RunContext {
        instructions_commit,
        repository_name,
    }
}

/// [`resolve_with`] with the default settings and a permit of its own.
#[cfg(test)]
pub async fn resolve(cwd: &Path) -> RunContext {
    let settings = ProbeSettings::default();
    let permits = Semaphore::new(1);
    let (_cancel, rx) = watch::channel(false);
    resolve_with(&settings, &permits, rx, cwd).await
}

fn is_full_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The command for one probe: git (or the test stand-in) with the ambient
/// location environment cleared and [`harden`] applied, run in `cwd`.
pub fn probe_command(
    settings: &ProbeSettings,
    cwd: &Path,
    args: &[&str],
) -> tokio::process::Command {
    let mut cmd = settings.program_command();
    cmd.arg("-C").arg(cwd);
    harden(&mut cmd);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    cmd
}

impl ProbeSettings {
    /// git, as [`crate::git_env::git_async`] builds it.
    #[cfg(not(test))]
    fn program_command(&self) -> tokio::process::Command {
        crate::git_env::git_async()
    }

    /// git, or the stand-in [`Self::program`] names, with the ambient
    /// location environment cleared as git's is.
    #[cfg(test)]
    fn program_command(&self) -> tokio::process::Command {
        match &self.program {
            None => crate::git_env::git_async(),
            Some((program, leading)) => {
                let mut cmd = tokio::process::Command::new(program);
                for var in crate::git_env::AMBIENT_LOCATION_VARS {
                    cmd.env_remove(var);
                }
                cmd.args(leading);
                cmd
            }
        }
    }
}

/// Switch off the settings and variables known to make a read-only git
/// command run another program — see the module docs for the Git version and
/// the trusted `git` this relies on. Global options only, so it goes before
/// the subcommand.
pub fn harden(cmd: &mut tokio::process::Command) {
    cmd.arg("--no-pager");
    for config in HARDENING_CONFIG {
        cmd.arg("-c").arg(config);
    }
    for (key, value) in HARDENING_ENV {
        cmd.env(key, value);
    }
    for key in HARDENING_ENV_REMOVED {
        cmd.env_remove(key);
    }
}

/// Run one probe: wait for a permit, then run it until it exits, the deadline
/// passes or `cancel` turns true; the last two kill and reap it. The whole
/// wait, permit included, is bounded by the deadline.
async fn probe(
    settings: &ProbeSettings,
    permits: &Semaphore,
    mut cancel: watch::Receiver<bool>,
    cwd: &Path,
    args: &[&str],
) -> Option<String> {
    if *cancel.borrow() {
        return None;
    }
    let deadline = tokio::time::Instant::now() + settings.deadline;
    let cancelled = async {
        let _ = cancel.wait_for(|c| *c).await;
    };
    tokio::pin!(cancelled);
    let _permit = tokio::select! {
        biased;
        _ = &mut cancelled => return None,
        permit = permits.acquire() => permit.ok()?,
        _ = tokio::time::sleep_until(deadline) => return None,
    };
    let mut child = probe_command(settings, cwd, args).spawn().ok()?;
    let stdout = child.stdout.take()?;
    let finished = {
        let run = async {
            let mut out = Vec::new();
            stdout
                .take(MAX_PROBE_OUTPUT)
                .read_to_end(&mut out)
                .await
                .ok()?;
            let status = child.wait().await.ok()?;
            Some((status, out))
        };
        tokio::select! {
            biased;
            _ = &mut cancelled => None,
            done = run => Some(done),
            _ = tokio::time::sleep_until(deadline) => None,
        }
    };
    let Some(done) = finished else {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(REAP_WAIT, child.wait()).await;
        return None;
    };
    let (status, out) = done?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8(out).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The repository name a remote URL ends in, with `.git` stripped:
/// `https://github.com/o/repo.git`, `git@github.com:o/repo` and `/srv/repo.git`
/// all give `repo`. Never the URL itself, which can carry a credential: only the
/// last path component is kept, and `None` comes back when there is no path
/// (`https://token@host`) or the component contains `@` or `:`.
pub fn repository_name_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim();
    let last = if url.contains("://") {
        let parsed = reqwest::Url::parse(url).ok()?;
        parsed
            .path_segments()?
            .rev()
            .find(|s| !s.is_empty())?
            .to_string()
    } else {
        // scp-like `user@host:owner/repo`, or a local path.
        let (_, path) = url
            .split_once(':')
            .filter(|_| !url.starts_with('/'))
            .unwrap_or(("", url));
        path.trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()?
            .to_string()
    };
    let name = last.strip_suffix(".git").unwrap_or(&last);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['@', ':'])
        || name.chars().any(char::is_control)
    {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_name_from_common_remote_shapes() {
        for (url, expected) in [
            (
                "https://github.com/vfarcic/dot-agent-deck.git",
                Some("dot-agent-deck"),
            ),
            (
                "https://github.com/vfarcic/dot-agent-deck",
                Some("dot-agent-deck"),
            ),
            (
                "https://github.com/vfarcic/dot-agent-deck/",
                Some("dot-agent-deck"),
            ),
            (
                "git@github.com:vfarcic/dot-agent-deck.git",
                Some("dot-agent-deck"),
            ),
            ("ssh://git@host:2222/team/repo.git", Some("repo")),
            ("https://user:tok@github.com/o/repo.git?x=1#f", Some("repo")),
            ("/srv/git/repo.git", Some("repo")),
            ("../sibling/repo/", Some("repo")),
            ("https://ghp_secret@github.com", None),
            ("https://github.com/", None),
            ("git@github.com:", None),
            ("", None),
        ] {
            assert_eq!(
                repository_name_from_remote_url(url).as_deref(),
                expected,
                "{url}"
            );
        }
    }

    #[test]
    fn full_hash_shapes() {
        assert!(is_full_hash(&"a".repeat(40)));
        assert!(is_full_hash(&"0".repeat(64)));
        assert!(!is_full_hash("abc123"));
        assert!(!is_full_hash(&"g".repeat(40)));
    }

    fn git(repo: &Path, sandbox: &Path, args: &[&str]) -> String {
        let out = crate::git_env::fixture_git(repo, sandbox)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "fixture git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with `CLAUDE.md` committed, and that commit's hash.
    fn repo_with_rules(scratch: &Path) -> (PathBuf, String) {
        let repo = scratch.join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, scratch, &["init", "--quiet"]);
        std::fs::write(repo.join("CLAUDE.md"), "rules").unwrap();
        git(&repo, scratch, &["add", "CLAUDE.md"]);
        git(&repo, scratch, &["commit", "--quiet", "-m", "rules"]);
        let head = git(&repo, scratch, &["rev-parse", "HEAD"]);
        (repo, head)
    }

    #[tokio::test]
    async fn resolves_the_instruction_commit_and_repository_name() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let repo = scratch.path().join("checkout");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, scratch.path(), &["init", "--quiet"]);

        // No history and no remote yet: both absent.
        assert_eq!(resolve(&repo).await, RunContext::default());

        std::fs::write(repo.join("CLAUDE.md"), "rules").unwrap();
        git(&repo, scratch.path(), &["add", "CLAUDE.md"]);
        git(&repo, scratch.path(), &["commit", "--quiet", "-m", "rules"]);
        let rules_commit = git(&repo, scratch.path(), &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("other.txt"), "x").unwrap();
        git(&repo, scratch.path(), &["add", "other.txt"]);
        git(
            &repo,
            scratch.path(),
            &["commit", "--quiet", "-m", "unrelated"],
        );
        let remote = "https://u:SECRET@example.com/team/my-repo.git";
        git(&repo, scratch.path(), &["remote", "add", "origin", remote]);
        // A rewrite rule is not applied: the name comes from the URL as
        // configured, not from what `insteadOf` turns it into.
        git(
            &repo,
            scratch.path(),
            &[
                "config",
                "url.https://example.com/other/rewritten.git.insteadOf",
                remote,
            ],
        );

        let context = resolve(&repo).await;
        assert_eq!(
            context.instructions_commit.as_deref(),
            Some(rules_commit.as_str()),
            "the commit that last touched an instruction path, not HEAD"
        );
        assert_eq!(context.repository_name.as_deref(), Some("my-repo"));
    }

    /// An agent working in a subdirectory of its repository gets the commit of
    /// the repository's own instruction files, not of paths under its
    /// directory.
    #[tokio::test]
    async fn a_run_in_a_subdirectory_reads_the_root_instructions() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (repo, rules_commit) = repo_with_rules(scratch.path());
        let nested = repo.join("crates").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("lib.rs"), "x").unwrap();
        git(&repo, scratch.path(), &["add", "crates"]);
        git(&repo, scratch.path(), &["commit", "--quiet", "-m", "code"]);
        assert_eq!(
            resolve(&nested).await.instructions_commit.as_deref(),
            Some(rules_commit.as_str())
        );
    }

    /// A repository's own `protocol.<name>.allow=always` does not reopen a
    /// transport for a probe, on any Git: the control shows that it overrides
    /// a bare `protocol.allow=never`.
    #[tokio::test]
    async fn a_repository_cannot_allow_a_transport_back() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (source, _) = repo_with_rules(scratch.path());
        let checkout = scratch.path().join("other");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, scratch.path(), &["init", "--quiet"]);
        git(
            &checkout,
            scratch.path(),
            &["config", "protocol.file.allow", "always"],
        );
        let url = format!("file://{}", source.display());
        let control = crate::git_env::fixture_git(&checkout, scratch.path())
            .args(["-c", "protocol.allow=never", "ls-remote", &url])
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "the repository's setting wins over the default alone: {}",
            String::from_utf8_lossy(&control.stderr)
        );
        // `output`, not `status`: the probe's stdout is piped, and a pipe no one
        // reads would fail the command for a reason of its own.
        let probed = probe_command(&ProbeSettings::default(), &checkout, &["ls-remote", &url])
            .output()
            .await
            .unwrap();
        assert!(
            !probed.status.success() && probed.stdout.is_empty(),
            "a probe reached a transport: {}",
            String::from_utf8_lossy(&probed.stdout)
        );
    }

    #[tokio::test]
    async fn a_missing_directory_resolves_to_nothing() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        assert_eq!(
            resolve(&scratch.path().join("missing")).await,
            RunContext::default()
        );
    }

    #[test]
    fn the_probe_command_is_hardened() {
        let cmd = probe_command(
            &ProbeSettings::default(),
            Path::new("/somewhere"),
            &["log", "--no-show-signature"],
        );
        let std = cmd.as_std();
        let args: Vec<String> = std
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let subcommand = args.iter().position(|a| a == "log").unwrap();
        for config in HARDENING_CONFIG {
            let at = args
                .windows(2)
                .position(|w| w[0] == "-c" && w[1] == config)
                .unwrap_or_else(|| panic!("-c {config} missing: {args:?}"));
            assert!(at < subcommand, "-c {config} must precede the subcommand");
        }
        for protocol in ["ext", "file", "ssh", "git", "http", "https"] {
            let config = format!("protocol.{protocol}.allow=never");
            assert!(
                args[..subcommand]
                    .windows(2)
                    .any(|w| w[0] == "-c" && w[1] == config),
                "-c {config} missing: {args:?}"
            );
        }
        assert!(args[..subcommand].contains(&"--no-pager".to_string()));
        let envs: std::collections::HashMap<String, Option<String>> = std
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for (key, value) in HARDENING_ENV {
            assert_eq!(envs.get(key), Some(&Some(value.to_string())), "{key}");
        }
        for key in HARDENING_ENV_REMOVED
            .iter()
            .chain(crate::git_env::AMBIENT_LOCATION_VARS.iter())
        {
            assert_eq!(envs.get(*key), Some(&None), "{key} is removed");
        }
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        crate::test_isolation::write_script(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A.2: a repository whose own configuration names programs for a
    /// signature check, fsmonitor and a pager, on a SIGNED instruction commit.
    /// Plain `git log` runs the verifier (the control proves the trap is
    /// armed); the deck's probe must run none of them.
    #[cfg(unix)]
    #[tokio::test]
    async fn configured_programs_never_run_from_a_probe() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (repo, unsigned) = repo_with_rules(scratch.path());
        let marker = scratch.path().join("MARKER");
        let trap = write_script(
            scratch.path(),
            "trap.sh",
            &format!("echo \"$0 $*\" >> '{}'\nexit 1", marker.display()),
        );
        // Re-create the commit with a signature header, so a configured
        // verifier has something to verify.
        let raw = git(&repo, scratch.path(), &["cat-file", "commit", &unsigned]);
        let (headers, message) = raw.split_once("\n\n").unwrap();
        let signed = format!(
            "{headers}\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEEfakefakefake\n -----END PGP SIGNATURE-----\n\n{message}\n"
        );
        let mut hash_object = crate::git_env::fixture_git(&repo, scratch.path())
            .args(["hash-object", "-t", "commit", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            hash_object
                .stdin
                .take()
                .unwrap()
                .write_all(signed.as_bytes())
                .unwrap();
        }
        let out = hash_object.wait_with_output().unwrap();
        assert!(out.status.success());
        let signed_commit = String::from_utf8(out.stdout).unwrap().trim().to_string();
        git(
            &repo,
            scratch.path(),
            &["update-ref", "HEAD", &signed_commit],
        );
        let trap = trap.to_string_lossy().into_owned();
        for (key, value) in [
            ("log.showSignature", "true"),
            ("gpg.program", trap.as_str()),
            ("gpg.ssh.program", trap.as_str()),
            ("gpg.x509.program", trap.as_str()),
            ("core.fsmonitor", trap.as_str()),
            ("core.pager", trap.as_str()),
            ("credential.helper", trap.as_str()),
        ] {
            git(&repo, scratch.path(), &["config", key, value]);
        }
        git(
            &repo,
            scratch.path(),
            &["remote", "add", "origin", "https://example.com/o/r.git"],
        );

        // The control: an ordinary `git log` with this configuration runs the
        // configured verifier.
        let _ = crate::git_env::fixture_git(&repo, scratch.path())
            .args(["log", "-1", "--format=%H", "--", "CLAUDE.md"])
            .output()
            .unwrap();
        assert!(marker.exists(), "the fixture's verifier trap is armed");
        std::fs::remove_file(&marker).unwrap();

        let context = resolve(&repo).await;
        assert!(
            !marker.exists(),
            "a probe ran a configured program: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
        assert_eq!(
            context.instructions_commit.as_deref(),
            Some(signed_commit.as_str()),
            "the probe still answered"
        );
        assert_eq!(context.repository_name.as_deref(), Some("r"));
    }

    #[cfg(unix)]
    fn pids(file: &Path) -> Vec<String> {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A stand-in git that records its pid and hangs.
    #[cfg(unix)]
    fn stalled(scratch: &Path, deadline: Duration) -> (ProbeSettings, PathBuf) {
        let pid_file = scratch.join("pids");
        let script = scratch.join("stalled-git.sh");
        std::fs::write(
            &script,
            format!("echo $$ >> '{}'\nexec sleep 30\n", pid_file.display()),
        )
        .unwrap();
        let settings = ProbeSettings {
            program: Some((
                PathBuf::from("/bin/sh"),
                vec![script.to_string_lossy().into_owned()],
            )),
            deadline,
            ..ProbeSettings::default()
        };
        (settings, pid_file)
    }

    #[cfg(unix)]
    async fn wait_for_pids(file: &Path, n: usize) {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while pids(file).len() < n {
            assert!(
                std::time::Instant::now() < until,
                "the stand-in never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A.3: a probe that hangs is killed and reaped at its deadline, and the
    /// run's context is simply absent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hanging_probe_is_killed_at_its_deadline() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (settings, pid_file) = stalled(scratch.path(), Duration::from_millis(300));
        let permits = Semaphore::new(1);
        let (_cancel, rx) = watch::channel(false);
        let started = std::time::Instant::now();
        let context = resolve_with(&settings, &permits, rx, scratch.path()).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the two deadlines: {:?}",
            started.elapsed()
        );
        assert_eq!(context, RunContext::default());
        let spawned = pids(&pid_file);
        assert_eq!(spawned.len(), 2, "both probes ran");
        for pid in spawned {
            assert!(!alive(&pid), "probe {pid} was killed and reaped");
        }
    }

    /// A.3: shutdown cancels outstanding probes — kills and reaps them — well
    /// inside its budget, whatever their own deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_cancels_outstanding_probes() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (settings, pid_file) = stalled(scratch.path(), Duration::from_secs(60));
        let probes = Probes::new(settings);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cwd = scratch.path().to_path_buf();
        probes.spawn(cwd.clone(), move |c| {
            let _ = tx.send(c);
        });
        wait_for_pids(&pid_file, 1).await;
        let started = std::time::Instant::now();
        probes.shutdown(Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(rx.recv().await, Some(RunContext::default()));
        for pid in pids(&pid_file) {
            assert!(!alive(&pid), "probe {pid} was killed and reaped");
        }
        assert_eq!(probes.outstanding(), 0);
        // After shutdown nothing new starts.
        probes.spawn(cwd, |_| panic!("no probe after shutdown"));
        assert_eq!(probes.outstanding(), 0);
    }

    /// A.3: at most `max_concurrent` git processes run at once, and at most
    /// `max_queued` runs wait.
    #[cfg(unix)]
    #[tokio::test]
    async fn probes_are_bounded_in_concurrency_and_queue() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (mut settings, pid_file) = stalled(scratch.path(), Duration::from_secs(60));
        settings.max_concurrent = 1;
        settings.max_queued = 2;
        let probes = Probes::new(settings);
        for _ in 0..5 {
            probes.spawn(scratch.path().to_path_buf(), |_| {});
        }
        assert_eq!(probes.outstanding(), 2, "the rest were left absent");
        wait_for_pids(&pid_file, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(pids(&pid_file).len(), 1, "one git at a time");
        probes.shutdown(Duration::from_secs(5)).await;
        for pid in pids(&pid_file) {
            assert!(!alive(&pid));
        }
    }

    /// A.3 / R2: a daemon whose runtime stops with a probe hanging exits
    /// promptly — the probes are async tasks, which a runtime drops, not
    /// blocking jobs it would wait for.
    #[cfg(unix)]
    #[test]
    fn a_hanging_probe_does_not_hold_the_runtime_open() {
        let scratch = crate::test_temp::tempdir().expect("scratch tempdir");
        let (settings, pid_file) = stalled(scratch.path(), Duration::from_secs(60));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let probes = runtime.block_on(async {
            let probes = Probes::new(settings);
            probes.spawn(scratch.path().to_path_buf(), |_| {});
            wait_for_pids(&pid_file, 1).await;
            probes
        });
        let started = std::time::Instant::now();
        drop(runtime);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "runtime shutdown waited on a probe: {:?}",
            started.elapsed()
        );
        drop(probes);
    }
}
