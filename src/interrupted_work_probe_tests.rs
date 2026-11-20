//! PRD #1790 stage 2: the interrupted work probes' deadline, against real
//! children that never finish on their own: a stand-in program that sleeps,
//! and a fake `gh` that does. What is under test is that the child and its
//! descendants are gone once the call returns, not only that the answer says
//! the probe failed.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use super::*;

/// A shell script at `path` that writes its own pid to `<dir>/leader.pid`,
/// starts a `sleep` in the background, writes that pid to `<dir>/child.pid`,
/// and waits for it: a stalled child with a descendant of its own.
fn stalled_script(path: &Path, dir: &Path) {
    let leader = dir.join("leader.pid");
    let child = dir.join("child.pid");
    crate::test_isolation::write_script(
        path,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\nwait\n",
            leader.display(),
            child.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn pid_in(file: &Path) -> Option<libc::pid_t> {
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// Whether `pid` no longer runs: no such process, or (on Linux) a zombie
/// waiting for a parent this test does not control to reap it.
fn gone(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let missing = unsafe { libc::kill(pid, 0) } != 0;
    missing || zombie(pid)
}

#[cfg(target_os = "linux")]
fn zombie(pid: libc::pid_t) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .unwrap_or_default()
        .rsplit_once(") ")
        .is_some_and(|(_, rest)| rest.starts_with('Z'))
}

#[cfg(not(target_os = "linux"))]
fn zombie(_pid: libc::pid_t) -> bool {
    false
}

/// Wait up to two seconds for `pid` to be gone.
fn soon_gone(pid: libc::pid_t) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(pid)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A probe whose child never finishes answers an error at its deadline, and
/// by the time it answers the child has been killed and reaped (so the
/// daemon-wide permit it held was held until then), and its background
/// descendant is killed with it.
#[test]
fn a_stalled_child_is_killed_and_reaped_by_its_deadline() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let script = tmp.path().join("stall");
        stalled_script(&script, tmp.path());
        let started = Instant::now();
        let answer = run(
            Program::Other(script.as_os_str()),
            tmp.path(),
            &[],
            Deadline::after(Duration::from_millis(800)),
        )
        .await;
        let took = started.elapsed();
        assert!(answer.is_err(), "a stalled child is no answer: {answer:?}");
        assert!(took < Duration::from_secs(5), "answered after {took:?}");
        let leader = pid_in(&tmp.path().join("leader.pid")).expect("the child started");
        assert!(
            gone(leader),
            "the child is reaped before the probe answers, with its permit"
        );
        let child = pid_in(&tmp.path().join("child.pid")).expect("the child forked");
        assert!(soon_gone(child), "its descendant is killed with its group");
        assert_eq!(
            PERMITS.available_permits(),
            PROBE_CONCURRENCY,
            "the permit is back once the probe answered"
        );
    });
}

/// A probe asked after its deadline starts nothing.
#[test]
fn a_probe_past_its_deadline_starts_nothing() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let script = tmp.path().join("stall");
        stalled_script(&script, tmp.path());
        let deadline = Deadline::after(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let answer = run(
            Program::Other(script.as_os_str()),
            tmp.path(),
            &[],
            deadline,
        )
        .await;
        assert!(answer.is_err());
        assert!(
            !tmp.path().join("leader.pid").exists(),
            "nothing was started"
        );
    });
}

/// The pull request lookup of an item whose repository has a GitHub
/// `origin`, against a fake `gh` that never answers: the state is unknown
/// once the lookup's deadline passes, and the fake `gh` and its descendant
/// are gone.
#[test]
fn a_stalled_gh_leaves_the_pull_request_unknown_and_is_gone_by_the_deadline() {
    rt().block_on(async {
        let tmp = crate::test_temp::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_env::fixture_git(&repo, &root)
                .args(args)
                .output()
                .expect("git available");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "."]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/someone/something.git",
        ]);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        stalled_script(&gh, &root);

        let started = Instant::now();
        let state = pr_state(
            &repo,
            "agent/dispatch-x",
            gh.as_os_str(),
            Deadline::after(Duration::from_secs(1)),
        )
        .await;
        let took = started.elapsed();
        assert_eq!(state, PrFact::Unknown);
        assert!(took < Duration::from_secs(6), "answered after {took:?}");
        let leader = pid_in(&root.join("leader.pid")).expect("the fake gh ran");
        assert!(gone(leader), "the fake gh is reaped before the answer");
        let child = pid_in(&root.join("child.pid")).expect("the fake gh forked");
        assert!(soon_gone(child), "and its descendant is killed");
    });
}

/// A deadline's remaining time is gone once it passes, and `min` keeps the
/// earlier one.
#[test]
fn a_deadline_runs_out_and_the_earlier_one_wins() {
    rt().block_on(async {
        let near = Deadline::after(Duration::from_millis(10));
        let far = Deadline::after(Duration::from_secs(60));
        assert_eq!(near.min(far), near);
        assert_eq!(far.min(near), near);
        assert!(far.remaining().is_some());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(near.remaining().is_none());
    });
}
