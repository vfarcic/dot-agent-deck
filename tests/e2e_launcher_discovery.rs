#![cfg(all(feature = "e2e", target_os = "linux"))]

//! Q35-22: discovery below an installed launcher, before production rollout.
//!
//! Assumed interfaces for the coder (not implemented by these tests):
//! * Q102b makes Q28(b)'s shared D1 resolver concrete: one resolution, at
//!   spawn, from the PATH the deck hands the PTY child when spawning the pane,
//!   recorded then, never anything the child sets later, following symlinks
//!   to a kernel identity (dev, ino).
//!   Every later check compares the candidate's /proc/<pid>/exe identity (plus
//!   birth identity, uid and ancestry) with that pinned pair and never looks
//!   a name up again. For an undeclared pane, independently pin the Claude and
//!   Codex native names at spawn; a later kind claim selects its existing pin.
//!   Missing resolution stays unavailable, never rescued by candidate claims.
//!   An old pane retains its old inode across an update, a wrapper exec of a
//!   different inode after its pin is refused, and a fresh pane pins the update.
//!   The wrapper is not handed a native executable path. Basename, argv,
//!   header and the candidate's own PATH cannot replace the independent pin.
//! * e2e-only `binding-status` returns read-only `composer_binding` with
//!   `generation`, `permission` ("pending", "armable", "inert") and `reason`
//!   (a refusal string pinned below). It cannot settle permission.
//!   Refusals distinguish "executable not resolved: expected <agent_type>",
//!   "executable differs: expected <agent_type>",
//!   "candidate left its parent", "candidate exited", "foreign owner" and
//!   "generation retired". Lifecycle/owner failures take precedence over an
//!   executable mismatch when the candidate is also a lookalike; a type claim
//!   changes the expected agent in the mismatch reason, never its authority.
//! * e2e-only `DOT_AGENT_DECK_E2E_DISCOVERY_GATE=discovery-gate` names one
//!   component under the sandbox HOME. With `phase=candidate`, pause after
//!   finding a name/argv candidate and before validating/publishing it. Write
//!   `<G>.arrived` JSON {generation,pid}, await `<G>.release` for at most 15 s,
//!   then perform normal validation (including uid, birth, live ancestry,
//!   executable and generation rechecks) and write `<G>.completed` after
//!   validation or retirement cancellation can publish nothing further. On
//!   retirement its JSON is {generation,reason:"generation retired"}; it must
//!   complete while .release is absent, rather than await release or expiry.
//!   Record `<G>.timed-out` on expiry; teardown cancels without granting anything.
//!   Gate and marker writes are restricted to the e2e sandbox HOME.
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
use live::{Fixture, Launcher, NativePins, WAIT};
use serde_json::{Value, json};
use spec::spec;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    LearnedLookalike,
    LearnedOtherKind,
}

impl Route {
    fn learned(self) -> bool {
        matches!(
            self,
            Self::Learned | Self::LearnedLookalike | Self::LearnedOtherKind
        )
    }

    fn lookalike(self) -> bool {
        matches!(
            self,
            Self::Lookalike | Self::LearnedLookalike | Self::LearnedOtherKind
        )
    }
}

fn fixture(kind: &'static str, route: Route, mode: &'static str, gate: bool, uid: bool) -> Fixture {
    fixture_with_pins(kind, route, mode, gate, uid, NativePins::Both)
}

fn fixture_with_pins(
    kind: &'static str,
    route: Route,
    mode: &'static str,
    gate: bool,
    uid: bool,
    pins: NativePins,
) -> Fixture {
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
            declared: !route.learned(),
            lookalike: route.lookalike(),
            pins,
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
    if route.learned() {
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
    launch_as(f, f.kind)
}

fn launch_as(f: &Fixture, claimed_kind: &str) -> Value {
    let ready = f.launch();
    // Ordinary authenticated hooks, using this fixture's initial capability.
    // A wrapper's type is learned through the same header/hook path as users.
    f.hook_event_as("session_start", claimed_kind);
    f.hook_event_as("idle", claimed_kind);
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
    bound_to(f, ready, executable_identity(f.native_executable()));
}

fn executable_identity(path: impl AsRef<std::path::Path>) -> (u64, u64) {
    let meta = fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

fn bound_to(f: &Fixture, ready: &Value, pinned: (u64, u64)) {
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
    assert_eq!(
        (a.exe.dev, a.exe.ino),
        pinned,
        "attest the spawn-time pinned kernel executable"
    );
    assert_eq!(executable_identity(format!("/proc/{pid}/exe")), pinned);
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

fn mismatch_reason(kind: &str) -> String {
    format!(
        "executable differs: expected {}",
        if kind == "codex" {
            "codex"
        } else {
            "claude_code"
        }
    )
}

fn denied(f: &Fixture, native_alive: bool, reason: &str) {
    let s = status(f);
    assert!(
        s["permission"] == "pending" || s["permission"] == "inert",
        "unattested candidate must not be Armable: {s}"
    );
    assert_eq!(
        s["reason"], reason,
        "discovery refusal must identify its cause: {s}"
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
            let retirement_budget =
                common::load_scaled(Duration::from_secs(3)).min(Duration::from_secs(12));
            // Include the synchronous stop request; successor startup must
            // never give a slow cancellation extra, unmeasured time.
            let retirement_started = Instant::now();
            let retirement_deadline = retirement_started + retirement_budget;
            f.replace_observing_retirement(|_| {
                assert!(!old_release.exists(), "old discovery gate must stay closed");
                assert!(
                    common::wait_until(
                        retirement_deadline.saturating_duration_since(Instant::now()),
                        || old_completed.exists()
                    ) && Instant::now() <= retirement_deadline,
                    "retired discovery must finish or cancel before release within {retirement_budget:?} of retirement, below the 15-second gate expiry; elapsed {:?}",
                    retirement_started.elapsed()
                );
                assert!(!old_release.exists(), "completion must not depend on release");
                let retired = live::read_json(&old_completed);
                assert_eq!(retired["generation"], old_generation);
                assert_eq!(retired["reason"], "generation retired");
                assert!(
                    !old_timed_out.exists(),
                    "retirement must cancel the paused discovery without waiting for its timeout"
                );
                assert!(
                    !old_instance.join(format!("gen-{old_generation}")).exists(),
                    "retired candidate cannot publish its old gen-G"
                );
            });
            assert_ne!(
                f.generation, old_generation,
                "replace must create a real successor"
            );
            pending(&f, route);
            let successor = launch(&f);
            arrived(&f, &successor);
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
            // Defensive teardown only, after cancellation and no publication.
            fs::write(old_release, "").unwrap();
            release(&f);
            if route.lookalike() {
                denied(&f, true, &mismatch_reason(kind));
            } else {
                bound(&f, &successor);
            }
            return;
        }
    }
    release(&f);
    let reason = match change {
        Change::Reparent => "candidate left its parent",
        Change::Exit => "candidate exited",
        Change::ForeignUid => "foreign owner",
        Change::Generation => unreachable!(),
    };
    denied(&f, !matches!(change, Change::Exit), reason);
}

fn lookalike(kind: &'static str, route: Route) {
    let f = fixture(kind, route, "spawn", true, false);
    pending(&f, route);
    let claimed_kind = if matches!(route, Route::LearnedOtherKind) {
        if kind == "codex" { "claude" } else { "codex" }
    } else {
        kind
    };
    let ready = launch_as(&f, claimed_kind);
    arrived(&f, &ready);
    let actual = executable_identity(format!("/proc/{}/exe", ready["pid"].as_u64().unwrap()));
    let trusted = executable_identity(f.native_executable());
    assert_ne!(
        actual, trusted,
        "the copy must really have a different inode"
    );
    if claimed_kind != kind {
        let other = f.native_executable().with_file_name(claimed_kind);
        assert_ne!(
            actual,
            executable_identity(other),
            "the other agent's independent pin also differs"
        );
    }
    release(&f);
    denied(&f, true, &mismatch_reason(claimed_kind));
}

fn missing_pin(kind: &'static str, route: Route, pins: NativePins, assert_refusal: bool) {
    // No candidate barrier is required: a missing pin may be refused before
    // discovery finds a candidate. The genuine process still makes its claim.
    let f = fixture_with_pins(kind, route, "spawn", false, false, pins);
    let ready = launch(&f);
    let actual = executable_identity(format!("/proc/{}/exe", ready["pid"].as_u64().unwrap()));
    assert_eq!(
        actual,
        executable_identity(f.native_executable()),
        "the genuine native copy claims its own kind"
    );
    let wrapper = live::read_json(&f.path("launcher-ready"));
    let spawn_path = wrapper["spawn_path"].as_str().unwrap();
    for name in ["claude", "codex"] {
        let resolved = std::env::split_paths(spawn_path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        });
        if name != kind && matches!(pins, NativePins::OtherOnly) {
            assert_eq!(
                executable_identity(resolved.expect("the unclaimed agent's pin exists")),
                executable_identity(f.native_executable().with_file_name(name)),
                "only the other agent's genuine identity resolves"
            );
        } else {
            assert_eq!(resolved, None, "the PTY child's spawn PATH lacks {name}");
        }
    }
    if assert_refusal {
        let reason = format!(
            "executable not resolved: expected {}",
            if kind == "codex" {
                "codex"
            } else {
                "claude_code"
            }
        );
        assert!(
            common::wait_until(WAIT, || status(&f)["reason"] == reason),
            "the missing spawn-time pin must have its own refusal reason: {}",
            status(&f)
        );
        denied(&f, true, &reason);
    } else {
        // Keep this control runnable while the read-only seam is still RED.
        let mut human = f.connect();
        human.write("MISSING-PIN-FIXTURE-ALIVE");
        f.expect(b"MISSING-PIN-FIXTURE-ALIVE");
    }
}

/// Scenario: Start a declared launcher with neither agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine claude native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_declared_missing_pin_cannot_get_permission() {
    missing_pin("claude", Route::Launcher, NativePins::Neither, true);
}

/// Scenario: Start an undeclared wrapper with neither agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine claude native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_missing_pin_cannot_get_permission() {
    missing_pin("claude", Route::Learned, NativePins::Neither, true);
}

/// Scenario: Start an undeclared wrapper with only the other agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine claude native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_other_pin_missing_pin_cannot_get_permission() {
    missing_pin("claude", Route::Learned, NativePins::OtherOnly, true);
}

/// Scenario: Start a declared launcher with neither agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine codex native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_declared_missing_pin_cannot_get_permission() {
    missing_pin("codex", Route::Launcher, NativePins::Neither, true);
}

/// Scenario: Start an undeclared wrapper with neither agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine codex native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_missing_pin_cannot_get_permission() {
    missing_pin("codex", Route::Learned, NativePins::Neither, true);
}

/// Scenario: Start an undeclared wrapper with only the other agent name on the PATH the deck passes to the PTY child at spawn, then launch the genuine codex native copy by absolute path and claim its own kind. The missing pin must retain its distinct refusal reason with no Armable permission, gen-G identity or helper check.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_other_pin_missing_pin_cannot_get_permission() {
    missing_pin("codex", Route::Learned, NativePins::OtherOnly, true);
}

#[derive(Clone, Copy)]
enum Update {
    Running,
    BeforeExec,
    Fresh,
}

fn replace_native(f: &Fixture) -> (u64, u64) {
    let original = executable_identity(f.native_executable());
    let replacement = f.native_executable().with_extension("updated");
    fs::copy(f.native_executable(), &replacement).unwrap();
    fs::rename(replacement, f.native_executable()).unwrap();
    let updated = executable_identity(f.native_executable());
    assert_ne!(
        original, updated,
        "rename a new inode over the same install path"
    );
    updated
}

fn update_history(kind: &'static str, route: Route, update: Update, gate: bool) {
    let mut f = fixture(kind, route, "spawn", gate, false);
    let original = executable_identity(f.native_executable());
    if gate {
        pending(&f, route);
    }
    let updated = if matches!(update, Update::Running) {
        None
    } else {
        Some(replace_native(&f))
    };
    if matches!(update, Update::Fresh) {
        f.replace();
        if gate {
            pending(&f, route);
        }
    }
    let ready = launch(&f);
    let process = format!("/proc/{}/exe", ready["pid"].as_u64().unwrap());
    let actual = executable_identity(&process);
    assert_eq!(
        actual,
        updated.unwrap_or(original),
        "the wrapper must exec the current install"
    );
    if gate {
        arrived(&f, &ready);
    }
    if matches!(update, Update::Running) {
        replace_native(&f);
        assert_eq!(
            executable_identity(&process),
            original,
            "a live old process retains its kernel executable"
        );
        assert!(
            fs::read_link(&process)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .ends_with(b" (deleted)"),
            "the old running inode must really be unlinked by the update"
        );
    }
    if gate {
        release(&f);
        if matches!(update, Update::BeforeExec) {
            denied(&f, true, &mismatch_reason(kind));
        } else {
            bound_to(
                &f,
                &ready,
                if matches!(update, Update::Fresh) {
                    updated.unwrap()
                } else {
                    original
                },
            );
        }
    } else {
        // Exercise the changed fixture even while binding-status is still RED.
        let mut human = f.connect();
        human.write("UPDATE-FIXTURE-ALIVE");
        f.expect(b"UPDATE-FIXTURE-ALIVE");
    }
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
    lookalike("claude", Route::Lookalike);
}

/// Scenario: An undeclared wrapper launches a claude lookalike and claims Claude through the pane's authenticated hooks. Discovery must refuse the different inode with a Claude-specific reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_lookalike_cannot_get_permission() {
    lookalike("claude", Route::LearnedLookalike);
}

/// Scenario: An undeclared wrapper launches a claude lookalike but claims Codex through the pane's authenticated hooks. Discovery must refuse it against the independently pinned Codex executable with a Codex-specific reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_lookalike_cannot_claim_codex() {
    lookalike("claude", Route::LearnedOtherKind);
}

/// Scenario: A native claude process is running when an update replaces its installation at the same path. Declared and learned panes must still bind their old pinned inode and check the touched empty composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_running_inode_survives_install_update() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("claude", route, Update::Running, true);
    }
}

/// Scenario: Replace claude at the same installation path after a pane starts but before its wrapper launches the native child. Declared and learned panes must refuse that new inode with an executable-mismatch reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_wrapper_exec_after_update_is_refused() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("claude", route, Update::BeforeExec, true);
    }
}

/// Scenario: Replace claude at the same installation path, then start a fresh pane generation before launching the native child. Declared and learned panes must bind the new inode and check the touched empty composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_fresh_spawn_pins_updated_inode() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("claude", route, Update::Fresh, true);
    }
}

/// Scenario: Pause claude launcher discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_reparented() {
    negative("claude", Route::Launcher, Change::Reparent);
}

/// Scenario: Pause claude launcher discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_exited() {
    negative("claude", Route::Launcher, Change::Exit);
}

/// Scenario: Pause claude launcher discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_foreign_uid() {
    negative("claude", Route::Launcher, Change::ForeignUid);
}

/// Scenario: Pause claude launcher discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_launcher_rejects_replaced_generation() {
    negative("claude", Route::Launcher, Change::Generation);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_reparented() {
    negative("claude", Route::Learned, Change::Reparent);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_exited() {
    negative("claude", Route::Learned, Change::Exit);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_foreign_uid() {
    negative("claude", Route::Learned, Change::ForeignUid);
}

/// Scenario: Pause claude learned wrapper discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_learned_wrapper_rejects_replaced_generation() {
    negative("claude", Route::Learned, Change::Generation);
}

/// Scenario: Pause claude lookalike discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_reparented() {
    negative("claude", Route::Lookalike, Change::Reparent);
}

/// Scenario: Pause claude lookalike discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_exited() {
    negative("claude", Route::Lookalike, Change::Exit);
}

/// Scenario: Pause claude lookalike discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_claude_lookalike_rejects_foreign_uid() {
    negative("claude", Route::Lookalike, Change::ForeignUid);
}

/// Scenario: Pause claude lookalike discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
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
    lookalike("codex", Route::Lookalike);
}

/// Scenario: An undeclared wrapper launches a codex lookalike and claims Codex through the pane's authenticated hooks. Discovery must refuse the different inode with a Codex-specific reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_lookalike_cannot_get_permission() {
    lookalike("codex", Route::LearnedLookalike);
}

/// Scenario: An undeclared wrapper launches a codex lookalike but claims Claude through the pane's authenticated hooks. Discovery must refuse it against the independently pinned Claude executable with a Claude-specific reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_lookalike_cannot_claim_claude() {
    lookalike("codex", Route::LearnedOtherKind);
}

/// Scenario: A native codex process is running when an update replaces its installation at the same path. Declared and learned panes must still bind their old pinned inode and check the touched empty composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_running_inode_survives_install_update() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("codex", route, Update::Running, true);
    }
}

/// Scenario: Replace codex at the same installation path after a pane starts but before its wrapper launches the native child. Declared and learned panes must refuse that new inode with an executable-mismatch reason and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_wrapper_exec_after_update_is_refused() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("codex", route, Update::BeforeExec, true);
    }
}

/// Scenario: Replace codex at the same installation path, then start a fresh pane generation before launching the native child. Declared and learned panes must bind the new inode and check the touched empty composer.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_fresh_spawn_pins_updated_inode() {
    for route in [Route::Launcher, Route::Learned] {
        update_history("codex", route, Update::Fresh, true);
    }
}

/// Scenario: Pause codex launcher discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_reparented() {
    negative("codex", Route::Launcher, Change::Reparent);
}

/// Scenario: Pause codex launcher discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_exited() {
    negative("codex", Route::Launcher, Change::Exit);
}

/// Scenario: Pause codex launcher discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_foreign_uid() {
    negative("codex", Route::Launcher, Change::ForeignUid);
}

/// Scenario: Pause codex launcher discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_launcher_rejects_replaced_generation() {
    negative("codex", Route::Launcher, Change::Generation);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_reparented() {
    negative("codex", Route::Learned, Change::Reparent);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_exited() {
    negative("codex", Route::Learned, Change::Exit);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_foreign_uid() {
    negative("codex", Route::Learned, Change::ForeignUid);
}

/// Scenario: Pause codex learned wrapper discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_learned_wrapper_rejects_replaced_generation() {
    negative("codex", Route::Learned, Change::Generation);
}

/// Scenario: Pause codex lookalike discovery before validation, then make the discovered native candidate leave its parent. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_reparented() {
    negative("codex", Route::Lookalike, Change::Reparent);
}

/// Scenario: Pause codex lookalike discovery before validation, then let the captured native candidate exit. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_exited() {
    negative("codex", Route::Lookalike, Change::Exit);
}

/// Scenario: Pause codex lookalike discovery before validation, then force the production validator's foreign-uid refusal. Resuming discovery must refuse with the specific cause, publish no identity and send no check bytes.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_foreign_uid() {
    negative("codex", Route::Lookalike, Change::ForeignUid);
}

/// Scenario: Pause codex lookalike discovery before validation, then replace the pane with a fresh installed generation. The retired discovery must complete or cancel with a generation-retired reason before its gate is released, without publishing; a genuine successor needs its own discovery.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_codex_lookalike_rejects_replaced_generation() {
    negative("codex", Route::Lookalike, Change::Generation);
}

/// Scenario: Start declared and learned claude wrappers, including same-kind and other-kind lookalike claims, in spawn and exec forms without a discovery seam, then invoke Ctrl+G with a human draft. The installed helper must run the user's editor and following input must reach the composer. Same-path updates and genuine absolute-path launches with missing spawn-time pins must preserve a live composer accepting human input.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_claude_wrappers_preserve_unarmed_editor() {
    for route in [
        Route::Launcher,
        Route::Learned,
        Route::Lookalike,
        Route::LearnedLookalike,
        Route::LearnedOtherKind,
    ] {
        for mode in ["spawn", "exec"] {
            let f = fixture("claude", route, mode, false, false);
            let claimed_kind = if matches!(route, Route::LearnedOtherKind) {
                if f.kind == "codex" { "claude" } else { "codex" }
            } else {
                f.kind
            };
            let ready = launch_as(&f, claimed_kind);
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
    for route in [Route::Launcher, Route::Learned] {
        for update in [Update::Running, Update::BeforeExec, Update::Fresh] {
            update_history("claude", route, update, false);
        }
        missing_pin("claude", route, NativePins::Neither, false);
    }
    missing_pin("claude", Route::Learned, NativePins::OtherOnly, false);
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

/// Scenario: Start declared and learned codex wrappers, including same-kind and other-kind lookalike claims, in spawn and exec forms without a discovery seam, then invoke Ctrl+G with a human draft. The installed helper must run the user's editor and following input must reach the composer. Same-path updates and genuine absolute-path launches with missing spawn-time pins must preserve a live composer accepting human input.
#[spec("inbox/probe/035")]
#[test]
fn probe_035_fixture_codex_wrappers_preserve_unarmed_editor() {
    for route in [
        Route::Launcher,
        Route::Learned,
        Route::Lookalike,
        Route::LearnedLookalike,
        Route::LearnedOtherKind,
    ] {
        for mode in ["spawn", "exec"] {
            let f = fixture("codex", route, mode, false, false);
            let claimed_kind = if matches!(route, Route::LearnedOtherKind) {
                if f.kind == "codex" { "claude" } else { "codex" }
            } else {
                f.kind
            };
            let ready = launch_as(&f, claimed_kind);
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
    for route in [Route::Launcher, Route::Learned] {
        for update in [Update::Running, Update::BeforeExec, Update::Fresh] {
            update_history("codex", route, update, false);
        }
        missing_pin("codex", route, NativePins::Neither, false);
    }
    missing_pin("codex", Route::Learned, NativePins::OtherOnly, false);
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
