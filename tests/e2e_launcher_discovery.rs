#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Q35-22: discovery below an installed launcher, before production rollout.
//!
//! Assumed interfaces for the coder (not implemented by these tests):
//! * Q28(b)'s shared D1 resolver pins the trusted native executable before
//!   considering process claims. Here the launch's private PATH resolves the
//!   explicit native program passed to the wrapper. A candidate's basename,
//!   argv or header alone is not authority; a different inode must fail.
//!   No late PATH lookup may rescue that mismatch.
//! * e2e-only `binding-status` returns read-only `composer_binding` with
//!   `generation`, `permission` ("pending", "armable", "inert") and `reason`
//!   (a nonempty string when discovery refuses). It cannot settle permission.
//! * e2e-only `DOT_AGENT_DECK_E2E_DISCOVERY_GATE=discovery-gate` names one
//!   component under the sandbox HOME. With `phase=candidate`, pause after
//!   finding a name/argv candidate and before validating/publishing it. Write
//!   `<G>.arrived` JSON {generation,pid}, await `<G>.release` for at most 15 s,
//!   then perform normal validation (including uid, birth, live ancestry,
//!   executable and generation rechecks) and write `<G>.completed` after
//!   validation or retirement cancellation can publish nothing further. Record
//!   `<G>.timed-out` on expiry; teardown cancels without granting anything.
//! * e2e-only `DOT_AGENT_DECK_E2E_DISCOVERY_REJECT_UID=1` forces ONLY the
//!   foreign-uid refusal at that production validator. It cannot supply an
//!   accepted uid or skip any other validation. This covers the fail-closed
//!   branch, not Linux's uid syscall: no second OS user or sudo is used.
//!
//! The existing editor-probe assumption supplies release measurement only;
//! the resolver, kernel identity checks and observer still decide binding.
//! S-20's `probe-request` uses the persistent production Schedule unchanged.
//! No operation installs in production, sets permission, supplies an empty
//! observation, publishes an arm, or resets the Schedule. No real agent runs.

mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;
#[path = "support/live_segments.rs"]
mod live;

use dot_agent_deck::composer_probe::classifier::Identity;
use dot_agent_deck::composer_probe::instance::decode_identity;
use live::{Fixture, Launcher, WAIT};
use serde_json::{Value, json};
use spec::spec;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

ctor::declarative::ctor! {
    #[ctor(unsafe)]
    fn native_fixture_entry() {
        live::native_entry();
    }
}

#[derive(Clone, Copy)]
enum Route {
    Launcher,
    Learned,
    Lookalike,
}

fn fixture(kind: &'static str, route: Route, mode: &'static str, gate: bool, uid: bool) -> Fixture {
    let mut env = Vec::new();
    if gate {
        env.push(("DOT_AGENT_DECK_E2E_DISCOVERY_GATE", "discovery-gate"));
    }
    if uid {
        env.push(("DOT_AGENT_DECK_E2E_DISCOVERY_REJECT_UID", "1"));
    }
    let f = Fixture::launcher(
        kind,
        Launcher {
            mode,
            declared: !matches!(route, Route::Learned),
            lookalike: matches!(route, Route::Lookalike),
        },
        &env,
    );
    if gate {
        fs::create_dir(gate_root(&f)).unwrap();
        fs::write(gate_root(&f).join("phase"), "candidate").unwrap();
    }
    f
}

fn gate_root(f: &Fixture) -> PathBuf {
    f.daemon.home.join("discovery-gate")
}

fn status(f: &Fixture) -> Value {
    let reply = f.control_for(f.generation, "binding-status");
    assert_eq!(
        reply["ok"], true,
        "Q35-22 requires read-only generation-bound binding-status: {reply}"
    );
    let binding = reply["composer_binding"].clone();
    assert_eq!(binding["generation"], f.generation, "{reply}");
    binding
}

fn identity(f: &Fixture) -> Option<Identity> {
    fs::read(f.instance().join(format!("gen-{}", f.generation)))
        .ok()
        .and_then(|bytes| decode_identity(&bytes))
}

fn unpublished(f: &Fixture) -> bool {
    !f.instance().join(format!("gen-{}", f.generation)).exists()
}

fn kind_file(f: &Fixture) -> Option<String> {
    fs::read_to_string(f.instance().join(format!("kind-{}", f.generation))).ok()
}

fn pending(f: &Fixture, route: Route) {
    assert_eq!(status(f)["permission"], "pending", "no native child yet");
    assert!(unpublished(f), "no identity before discovery");
    if matches!(route, Route::Learned) {
        assert_eq!(
            kind_file(f),
            None,
            "absent kind-G means learned, not Claude"
        );
    } else {
        assert_eq!(kind_file(f), Some(format!("{}\n", f.kind)));
    }
}

fn launch(f: &Fixture) -> Value {
    let ready = f.launch();
    // Ordinary authenticated hooks, using this fixture's initial capability.
    // A wrapper's type is learned through the same header/hook path as users.
    f.hook_event("session_start");
    f.hook_event("idle");
    ready
}

fn arrived(f: &Fixture, ready: &Value) {
    let event = live::read_json(&gate_root(f).join(format!("{}.arrived", f.generation)));
    assert_eq!(event["generation"], f.generation);
    assert_eq!(
        event["pid"], ready["pid"],
        "pause the actual candidate, not the wrapper"
    );
    assert!(
        !gate_root(f)
            .join(format!("{}.timed-out", f.generation))
            .exists()
    );
    assert!(
        unpublished(f),
        "the barrier must precede identity publication"
    );
}

fn release(f: &Fixture) {
    fs::write(gate_root(f).join(format!("{}.release", f.generation)), "").unwrap();
    let completed = gate_root(f).join(format!("{}.completed", f.generation));
    assert!(
        common::wait_until(WAIT, || completed.exists()),
        "discovery never completed for generation {}",
        f.generation
    );
    assert!(
        !gate_root(f)
            .join(format!("{}.timed-out", f.generation))
            .exists(),
        "a timed-out gate cannot stand in for the requested ordering"
    );
}

fn native_start(pid: u64) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap()
}

fn check(f: &Fixture) {
    let mut human = f.connect();
    human.write("x\x7f");
    f.expect(b"x\x7f");
    let reply = f.probe();
    assert_eq!(reply["composer_probe"]["outcome"], "written", "{reply}");
    f.wait("helper-exited-1");
    f.expect(b"x\x7f\x07");
    assert!(
        !f.path("editor-started").exists(),
        "a checked empty box must skip the user's editor"
    );
    assert!(
        common::wait_until(WAIT, || {
            let (epoch, observed, _) = f.epoch();
            epoch > 0 && observed == epoch
        }),
        "the real installed helper's empty observation must settle"
    );
    f.stable(b"x\x7f\x07");
}

fn bound(f: &Fixture, ready: &Value) {
    assert!(
        common::wait_until(WAIT, || status(f)["permission"] == "armable"),
        "the attested native agent must become Armable: {}",
        status(f)
    );
    let a = identity(f).expect("Armable must publish gen-G first");
    let pid = ready["pid"].as_u64().unwrap();
    assert_eq!(
        u64::from(a.pid),
        pid,
        "A is the native agent, not its launcher"
    );
    assert_eq!(a.start, native_start(pid), "pin native birth identity");
    let meta = fs::metadata(f.native_executable()).unwrap();
    assert_eq!(
        (a.exe.dev, a.exe.ino),
        (meta.dev(), meta.ino()),
        "attest the native kernel executable"
    );
    assert_eq!(kind_file(f), Some(format!("{}\n", f.kind)));
    for prefix in ["gen", "kind"] {
        let mode = fs::metadata(f.instance().join(format!("{prefix}-{}", f.generation)))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o400, "owner-read-only identity file: {prefix}");
    }
    let wrapper = live::read_json(&f.path("launcher-ready"));
    assert_eq!(
        ready["editors"], wrapper["editors"],
        "binding changes no installed child's editor environment"
    );
    // Read only this acknowledged native child's initial environment. Never
    // print the environment: it also contains this sandbox's capability.
    let initial = fs::read(format!("/proc/{pid}/environ")).unwrap();
    check(f);
    assert!(
        fs::read(format!("/proc/{pid}/environ")).unwrap() == initial,
        "native spawn environment remains byte-identical"
    );
}

fn denied(f: &Fixture, native_alive: bool) {
    let s = status(f);
    assert!(
        s["permission"] == "pending" || s["permission"] == "inert",
        "unattested candidate must not be Armable: {s}"
    );
    assert!(
        s["reason"].as_str().is_some_and(|s| !s.trim().is_empty()),
        "refused discovery carries its reason: {s}"
    );
    assert!(unpublished(f), "refused candidate must not publish gen-G");
    if native_alive {
        let mut human = f.connect();
        human.write("x\x7f");
    }
    f.expect(b"x\x7f");
    let reply = f.probe();
    assert_ne!(reply["composer_probe"]["outcome"], "written", "{reply}");
    if native_alive {
        f.stable(b"x\x7f");
    }
    assert!(
        !f.path("helper-started-1").exists(),
        "refused binding cannot run a helper check"
    );
}

fn positive(kind: &'static str, route: Route, mode: &'static str) {
    let f = fixture(kind, route, mode, true, false);
    pending(&f, route);
    let ready = launch(&f);
    let wrapper = live::read_json(&f.path("launcher-ready"));
    if mode == "exec" {
        assert_eq!(
            ready["pid"], wrapper["pid"],
            "exec retains the PTY child's pid"
        );
    } else {
        assert_ne!(
            ready["pid"], wrapper["pid"],
            "spawn binds below the wrapper"
        );
    }
    arrived(&f, &ready);
    assert_eq!(
        status(&f)["permission"],
        "pending",
        "a discovered candidate isn't yet attested"
    );
    release(&f);
    bound(&f, &ready);
}

#[derive(Clone, Copy)]
enum Change {
    Reparent,
    Exit,
    ForeignUid,
    Generation,
}

fn negative(kind: &'static str, route: Route, change: Change) {
    let mut f = fixture(
        kind,
        route,
        if matches!(change, Change::Reparent) {
            "reparent"
        } else {
            "spawn"
        },
        true,
        matches!(change, Change::ForeignUid),
    );
    pending(&f, route);
    let ready = launch(&f);
    arrived(&f, &ready);
    match change {
        Change::Reparent => {
            let parent = live::read_json(&f.path("parent-ready"));
            assert_eq!(parent["child"], ready["pid"]);
            f.flag("reparent-go");
            let pid = ready["pid"].as_u64().unwrap();
            assert!(
                common::wait_until(WAIT, || {
                    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
                        return false;
                    };
                    let ppid = stat[stat.rfind(')').unwrap() + 1..]
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .parse::<u64>()
                        .unwrap();
                    ppid != parent["pid"].as_u64().unwrap()
                }),
                "the candidate must really leave its captured parent"
            );
        }
        Change::Exit => {
            let mut human = f.connect();
            human.write("x\x7f");
            f.expect(b"x\x7f");
            f.flag("native-stop");
            f.wait("native-stopped");
            let pid = ready["pid"].as_u64().unwrap();
            assert!(
                common::wait_until(WAIT, || {
                    fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
                        stat[stat.rfind(')').unwrap() + 1..]
                            .split_whitespace()
                            .next()
                            == Some("Z")
                    })
                }),
                "the captured native child must really exit before validation resumes"
            );
        }
        Change::ForeignUid => {}
        Change::Generation => {
            let old_generation = f.generation;
            let old_instance = f.instance().to_owned();
            let old_release = gate_root(&f).join(format!("{old_generation}.release"));
            let old_completed = gate_root(&f).join(format!("{old_generation}.completed"));
            let old_timed_out = gate_root(&f).join(format!("{old_generation}.timed-out"));
            f.replace();
            assert_ne!(
                f.generation, old_generation,
                "replace must create a real successor"
            );
            pending(&f, route);
            let successor = launch(&f);
            arrived(&f, &successor);
            fs::write(old_release, "").unwrap();
            assert!(
                common::wait_until(WAIT, || old_completed.exists()),
                "retired discovery must finish or cancel before checking for stale publication"
            );
            assert!(
                !old_timed_out.exists(),
                "retirement must cancel the paused discovery without waiting for its timeout"
            );
            assert_eq!(
                f.control_for(old_generation, "binding-status")["ok"],
                false,
                "stale discovery can't address its successor"
            );
            assert!(
                !old_instance.join(format!("gen-{old_generation}")).exists(),
                "retired candidate cannot publish its old gen-G"
            );
            assert_eq!(
                status(&f)["permission"],
                "pending",
                "old discovery can't grant successor permission"
            );
            assert!(unpublished(&f));
            release(&f);
            if matches!(route, Route::Lookalike) {
                denied(&f, true);
            } else {
                bound(&f, &successor);
            }
            return;
        }
    }
    release(&f);
    denied(&f, !matches!(change, Change::Exit));
}

/// Scenario: Start a launcher exec with a delayed native claude stand-in, then let it exec the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_launcher_exec_binds_native_claude_and_checks() {
    positive("claude", Route::Launcher, "exec");
}

/// Scenario: Start a launcher spawn with a delayed native claude stand-in, then let it spawn the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_launcher_spawn_binds_native_claude_and_checks() {
    positive("claude", Route::Launcher, "spawn");
}

/// Scenario: Start a learned wrapper with a delayed native claude stand-in, then let it spawn the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_learned_wrapper_binds_native_claude_and_checks() {
    positive("claude", Route::Learned, "spawn");
}

/// Scenario: A declared launcher spawns a claude lookalike with the trusted program's bytes, basename and claimed argv, but a different executable inode. Discovery must retain a reason without publishing an identity or sending Ctrl+G for a check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_cannot_get_permission() {
    let f = fixture("claude", Route::Lookalike, "spawn", true, false);
    pending(&f, Route::Lookalike);
    let ready = launch(&f);
    arrived(&f, &ready);
    let actual = fs::metadata(ready["exe"].as_str().unwrap()).unwrap();
    let trusted = fs::metadata(f.native_executable()).unwrap();
    assert_ne!((actual.dev(), actual.ino()), (trusted.dev(), trusted.ino()));
    release(&f);
    denied(&f, true);
}

/// Scenario: Pause claude launcher discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_reparented() {
    negative("claude", Route::Launcher, Change::Reparent);
}

/// Scenario: Pause claude launcher discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_exited() {
    negative("claude", Route::Launcher, Change::Exit);
}

/// Scenario: Pause claude launcher discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_foreign_uid() {
    negative("claude", Route::Launcher, Change::ForeignUid);
}

/// Scenario: Pause claude launcher discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_replaced_generation() {
    negative("claude", Route::Launcher, Change::Generation);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_reparented() {
    negative("claude", Route::Learned, Change::Reparent);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_exited() {
    negative("claude", Route::Learned, Change::Exit);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_foreign_uid() {
    negative("claude", Route::Learned, Change::ForeignUid);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_replaced_generation() {
    negative("claude", Route::Learned, Change::Generation);
}

/// Scenario: Pause claude lookalike discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_reparented() {
    negative("claude", Route::Lookalike, Change::Reparent);
}

/// Scenario: Pause claude lookalike discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_exited() {
    negative("claude", Route::Lookalike, Change::Exit);
}

/// Scenario: Pause claude lookalike discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_foreign_uid() {
    negative("claude", Route::Lookalike, Change::ForeignUid);
}

/// Scenario: Pause claude lookalike discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_replaced_generation() {
    negative("claude", Route::Lookalike, Change::Generation);
}

/// Scenario: Start a launcher exec with a delayed native codex stand-in, then let it exec the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_launcher_exec_binds_native_codex_and_checks() {
    positive("codex", Route::Launcher, "exec");
}

/// Scenario: Start a launcher spawn with a delayed native codex stand-in, then let it spawn the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_launcher_spawn_binds_native_codex_and_checks() {
    positive("codex", Route::Launcher, "spawn");
}

/// Scenario: Start a learned wrapper with a delayed native codex stand-in, then let it spawn the agent and finish discovery. The native child's identity and kind must be published, its editor environment preserved, and the real installed helper must check its touched empty box.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_learned_wrapper_binds_native_codex_and_checks() {
    positive("codex", Route::Learned, "spawn");
}

/// Scenario: A declared launcher spawns a codex lookalike with the trusted program's bytes, basename and claimed argv, but a different executable inode. Discovery must retain a reason without publishing an identity or sending Ctrl+G for a check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_cannot_get_permission() {
    let f = fixture("codex", Route::Lookalike, "spawn", true, false);
    pending(&f, Route::Lookalike);
    let ready = launch(&f);
    arrived(&f, &ready);
    let actual = fs::metadata(ready["exe"].as_str().unwrap()).unwrap();
    let trusted = fs::metadata(f.native_executable()).unwrap();
    assert_ne!((actual.dev(), actual.ino()), (trusted.dev(), trusted.ino()));
    release(&f);
    denied(&f, true);
}

/// Scenario: Pause codex launcher discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_reparented() {
    negative("codex", Route::Launcher, Change::Reparent);
}

/// Scenario: Pause codex launcher discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_exited() {
    negative("codex", Route::Launcher, Change::Exit);
}

/// Scenario: Pause codex launcher discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_foreign_uid() {
    negative("codex", Route::Launcher, Change::ForeignUid);
}

/// Scenario: Pause codex launcher discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_replaced_generation() {
    negative("codex", Route::Launcher, Change::Generation);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_reparented() {
    negative("codex", Route::Learned, Change::Reparent);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_exited() {
    negative("codex", Route::Learned, Change::Exit);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_foreign_uid() {
    negative("codex", Route::Learned, Change::ForeignUid);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_replaced_generation() {
    negative("codex", Route::Learned, Change::Generation);
}

/// Scenario: Pause codex lookalike discovery before validation, then make the discovered native candidate leave its parent. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_reparented() {
    negative("codex", Route::Lookalike, Change::Reparent);
}

/// Scenario: Pause codex lookalike discovery before validation, then let the captured native candidate exit. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_exited() {
    negative("codex", Route::Lookalike, Change::Exit);
}

/// Scenario: Pause codex lookalike discovery before validation, then force the production validator's foreign-uid refusal. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_foreign_uid() {
    negative("codex", Route::Lookalike, Change::ForeignUid);
}

/// Scenario: Pause codex lookalike discovery before validation, then replace the pane with a fresh installed generation. Resuming the old discovery must not publish its candidate or run a check; a genuine successor must use its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_replaced_generation() {
    negative("codex", Route::Lookalike, Change::Generation);
}

/// Scenario: Start declared, learned and lookalike claude wrappers in both spawn and exec forms without a discovery seam, then invoke Ctrl+G with a human draft. The genuine installed helper must run the user's editor with that draft, and following input must still reach the composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_claude_wrappers_preserve_unarmed_editor() {
    for route in [Route::Launcher, Route::Learned, Route::Lookalike] {
        for mode in ["spawn", "exec"] {
            let f = fixture("claude", route, mode, false, false);
            let ready = launch(&f);
            let wrapper = live::read_json(&f.path("launcher-ready"));
            assert_eq!(ready["editors"], wrapper["editors"]);
            let mut human = f.connect();
            human.write("human-draft");
            f.expect(b"human-draft");
            human.write("\x07");
            let editor = live::read_json(&f.path("editor-started"));
            assert_eq!(editor["hex"], "68756d616e2d6472616674");
            assert_eq!(editor["tty"], json!([true, true, true]));
            f.wait("helper-exited-1");
            assert!(
                common::wait_until(WAIT, || f.screen().contains("EDITED")),
                "wait for the composer's visible editor return before typing after Codex's flush"
            );
            human.write("AFTER");
            f.expect(b"human-draft\x07AFTER");
        }
    }
}

/// Scenario: Start a private intermediate wrapper with a native claude child and release only that intermediate parent. The native process must leave the captured parent, remain visible and accept human input before its owned fixture is stopped.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_claude_can_reparent_a_live_candidate() {
    let f = fixture("claude", Route::Launcher, "reparent", false, false);
    let ready = launch(&f);
    let parent = live::read_json(&f.path("parent-ready"));
    assert_eq!(parent["child"], ready["pid"]);
    f.flag("reparent-go");
    let pid = ready["pid"].as_u64().unwrap();
    assert!(common::wait_until(WAIT, || {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat[stat.rfind(')').unwrap() + 1..]
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            != parent["pid"].as_u64().unwrap()
    }));
    let mut human = f.connect();
    human.write("ORPHAN-ALIVE");
    f.expect(b"ORPHAN-ALIVE");
    f.flag("native-stop");
    f.wait("native-stopped");
}

/// Scenario: Start declared, learned and lookalike codex wrappers in both spawn and exec forms without a discovery seam, then invoke Ctrl+G with a human draft. The genuine installed helper must run the user's editor with that draft, and following input must still reach the composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_codex_wrappers_preserve_unarmed_editor() {
    for route in [Route::Launcher, Route::Learned, Route::Lookalike] {
        for mode in ["spawn", "exec"] {
            let f = fixture("codex", route, mode, false, false);
            let ready = launch(&f);
            let wrapper = live::read_json(&f.path("launcher-ready"));
            assert_eq!(ready["editors"], wrapper["editors"]);
            let mut human = f.connect();
            human.write("human-draft");
            f.expect(b"human-draft");
            human.write("\x07");
            let editor = live::read_json(&f.path("editor-started"));
            assert_eq!(editor["hex"], "68756d616e2d6472616674");
            assert_eq!(editor["tty"], json!([true, true, true]));
            f.wait("helper-exited-1");
            assert!(
                common::wait_until(WAIT, || f.screen().contains("EDITED")),
                "wait for the composer's visible editor return before typing after Codex's flush"
            );
            human.write("AFTER");
            f.expect(b"human-draft\x07AFTER");
        }
    }
}

/// Scenario: Start a private intermediate wrapper with a native codex child and release only that intermediate parent. The native process must leave the captured parent, remain visible and accept human input before its owned fixture is stopped.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_codex_can_reparent_a_live_candidate() {
    let f = fixture("codex", Route::Launcher, "reparent", false, false);
    let ready = launch(&f);
    let parent = live::read_json(&f.path("parent-ready"));
    assert_eq!(parent["child"], ready["pid"]);
    f.flag("reparent-go");
    let pid = ready["pid"].as_u64().unwrap();
    assert!(common::wait_until(WAIT, || {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        stat[stat.rfind(')').unwrap() + 1..]
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            != parent["pid"].as_u64().unwrap()
    }));
    let mut human = f.connect();
    human.write("ORPHAN-ALIVE");
    f.expect(b"ORPHAN-ALIVE");
    f.flag("native-stop");
    f.wait("native-stopped");
}
