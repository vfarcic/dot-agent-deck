//! Issue #648: the Rust toolchain and `cargo-nextest` versions are pinned in
//! **two** places — `devbox.json`, which is what a `devbox shell` installs, and
//! `.github/workflows/`, which is what CI installs — and the whole point of
//! pinning them is that `cargo test-fast` locally and `cargo nextest run` in CI
//! are the same claim. Nothing checked that, and on 2026-08-11 they diverged:
//! an automerged Renovate PR moved `devbox.json` to cargo-nextest 0.9.143 while
//! `ci.yml` stayed on 0.9.140 for eleven days.
//!
//! `scripts/check-pin-lockstep.sh` is the check. These tests are what stop it
//! being a no-op:
//!
//! - one runs it against the **real repository**, which is the guard itself —
//!   it is why a drifted pin turns `cargo test-fast` red on a contributor's
//!   machine and in the three required CI build jobs, rather than waiting for
//!   somebody to diff two files by hand;
//! - the rest drive it against synthetic drifted trees, because a guard whose
//!   failure path is never exercised is indistinguishable from `exit 0`. That
//!   is not a hypothetical worry here: CLAUDE.md rule 5 exists because these
//!   very crates' runtime assertions ran in no gate anywhere for months.
//!
//! Unix-only. The script needs a POSIX shell, the pins it guards are consumed
//! by a devbox that has no Windows support, and the check already reaches
//! `build` and `build-macos` plus the `devbox` job — so gating it here costs
//! nothing and avoids making a Git-Bash path translation the difference
//! between a green and a red `build-windows`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The workspace root, from this crate's manifest dir rather than the process
/// cwd, so the tests do not depend on how the runner was invoked.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/check-pin-lockstep.sh")
}

/// Same shape as `verify_pr_stream`'s probe: say so loudly rather than failing
/// a contributor's unrelated change on a missing interpreter.
fn bash_present() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_against(root: &Path) -> Output {
    Command::new("bash")
        .arg(script())
        .arg(root)
        .output()
        .expect("run scripts/check-pin-lockstep.sh")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A synthetic repository holding nothing but the two files the script reads.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// `packages` are devbox.json entries verbatim; `workflows` are
    /// `(file name, contents)` pairs written under `.github/workflows/`.
    fn new(packages: &[&str], workflows: &[(&str, String)]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir for the fixture repository");
        let quoted: Vec<String> = packages.iter().map(|p| format!("    \"{p}\"")).collect();
        fs::write(
            dir.path().join("devbox.json"),
            format!("{{\n  \"packages\": [\n{}\n  ]\n}}\n", quoted.join(",\n")),
        )
        .expect("write the fixture devbox.json");

        let wf = dir.path().join(".github/workflows");
        fs::create_dir_all(&wf).expect("create the fixture workflow dir");
        for (name, body) in workflows {
            fs::write(wf.join(name), body).expect("write a fixture workflow");
        }
        Self { dir }
    }

    fn run(&self) -> Output {
        run_against(self.dir.path())
    }
}

/// devbox.json entries that agree with [`workflow`]'s defaults.
fn good_packages() -> Vec<&'static str> {
    vec![
        "jq@1.8.2",
        "cargo-nextest@0.9.143",
        "rustc@1.97.1",
        "cargo@1.97.1",
        "clippy@1.97.1",
        "rustfmt@1.97.1",
    ]
}

/// A minimal workflow carrying one of each pin, in the exact spelling the real
/// files and Renovate's customManagers use.
fn workflow(toolchain: &str, nextest: &str) -> String {
    format!(
        "jobs:\n  \
         build:\n    \
         steps:\n      \
         - uses: dtolnay/rust-toolchain@v1\n        \
         with:\n          \
         toolchain: {toolchain}\n      \
         - uses: taiki-e/install-action@v2\n        \
         with:\n          \
         tool: cargo-nextest@{nextest}\n"
    )
}

fn good_workflows() -> Vec<(&'static str, String)> {
    vec![("ci.yml", workflow("1.97.1", "0.9.143"))]
}

/// THE guard. Everything else in this file exists to prove this assertion can
/// fail; this is the one that runs against what actually ships.
#[test]
fn repository_pins_are_in_lockstep() {
    if !bash_present() {
        eprintln!("SKIP: the pin-lockstep guard needs `bash` on PATH");
        return;
    }
    let out = run_against(&repo_root());
    assert!(
        out.status.success(),
        "devbox.json and .github/workflows/ disagree about a pinned toolchain \
         version — `cargo test-fast` in a devbox shell and `cargo nextest run` \
         in CI would run different builds (issue #648):\n{}",
        combined(&out)
    );
}

#[test]
fn agreeing_fixture_passes() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::new(&good_packages(), &good_workflows()).run();
    assert!(
        out.status.success(),
        "a fixture whose two sides agree must pass, or every failure below \
         proves nothing about drift:\n{}",
        combined(&out)
    );
}

#[test]
fn drifted_nextest_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The exact shape of the reported bug: devbox ahead, workflow behind.
    let out = Fixture::new(
        &good_packages(),
        &[("ci.yml", workflow("1.97.1", "0.9.140"))],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "0.9.143 vs 0.9.140 must fail:\n{text}"
    );
    assert!(
        text.contains("cargo-nextest") && text.contains("0.9.140") && text.contains("0.9.143"),
        "the failure must name the class and BOTH versions, or it cannot be \
         acted on without opening the files:\n{text}"
    );
}

#[test]
fn drifted_toolchain_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The hazard PR #647 is held open to avoid: workflows moved to a Rust
    // nixpkgs does not carry yet, devbox left behind.
    let out = Fixture::new(
        &good_packages(),
        &[("ci.yml", workflow("1.98.0", "0.9.143"))],
    )
    .run();
    let text = combined(&out);
    assert!(!out.status.success(), "1.97.1 vs 1.98.0 must fail:\n{text}");
    assert!(
        text.contains("Rust toolchain") && text.contains("1.98.0"),
        "the failure must name the class and the version:\n{text}"
    );
}

#[test]
fn one_drifted_workflow_among_several_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Seven `toolchain:` sites live across three files today, so "all but one
    // agree" is the realistic way a half-applied bump looks.
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("release.yml", workflow("1.97.1", "0.9.143")),
            (
                "aarch64-crossbuild-check.yml",
                workflow("1.98.0", "0.9.143"),
            ),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a half-applied bump must fail:\n{text}"
    );
    assert!(
        text.contains("aarch64-crossbuild-check.yml"),
        "the failure must point at the file that was missed:\n{text}"
    );
}

#[test]
fn inconsistent_devbox_rust_components_fail() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // rustc/cargo/clippy/rustfmt are four packages carrying ONE toolchain; a
    // bump that moves three of them is its own kind of broken.
    let out = Fixture::new(
        &[
            "cargo-nextest@0.9.143",
            "rustc@1.98.0",
            "cargo@1.98.0",
            "clippy@1.98.0",
            "rustfmt@1.97.1",
        ],
        &good_workflows(),
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a devbox toolchain split across two versions must fail:\n{text}"
    );
    assert!(
        text.contains("internally inconsistent"),
        "the failure must say the inconsistency is inside devbox.json, not \
         between the two sides:\n{text}"
    );
}

#[test]
fn reformatted_toolchain_pin_fails_instead_of_vanishing() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The silent-rot class PR #641 named: Renovate finds these pins with a
    // regex over a bare X.Y.Z, so a quoted value stops being tracked without
    // anything going red. Here it goes red.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: \"1.97.x\"\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest@0.9.143\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a pin Renovate's regex cannot read must fail loudly, not silently \
         stop being a pin:\n{text}"
    );
    assert!(
        text.contains("unreadable"),
        "the failure must say the pin is unreadable rather than merely \
         mismatched:\n{text}"
    );
}

#[test]
fn an_unreadable_pin_fails_even_when_every_readable_pin_agrees() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Regression: the scanners run inside `$(...)`, so a subshell setting the
    // failure flag loses it. Caught while writing these tests — the script
    // printed DRIFT for the unreadable pin and then exited 0, because the ONE
    // readable toolchain site agreed with devbox.json and nothing carried the
    // subshell's finding back. A guard that reports a problem and exits clean
    // is worse than no guard: CI stays green and the message scrolls past.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: \"1.97.x\"\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest@0.9.143\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", body.to_string()),
            ("release.yml", workflow("1.97.1", "0.9.143")),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "an unreadable pin must set the exit code, not merely print:\n{text}"
    );
}

#[test]
fn unpinned_nextest_tool_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // `taiki-e/install-action` accepts a bare tool name and floats to latest —
    // which is what ci.yml deliberately moved away from on 2026-08-06.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: 1.97.1\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(!out.status.success(), "a floating tool must fail:\n{text}");
    assert!(
        text.contains("no version"),
        "the failure must say the pin is missing:\n{text}"
    );
}

#[test]
fn a_side_with_no_pins_at_all_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The vacuous-pass guard. If a rename ever made both regexes match nothing,
    // "the versions agree" would be true and worthless.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: 1.97.1\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a class pinned in devbox.json but in no workflow must fail rather \
         than pass by absence:\n{text}"
    );
    assert!(
        text.contains("no workflow pins this class"),
        "the failure must name absence as the cause:\n{text}"
    );
}

#[test]
fn shell_expansions_and_comments_are_not_pins() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Both live in the real ci.yml: `windows-cross-check` echoes a resolved
    // rustup directory as `toolchain: $…` (diagnostics, not a version), and the
    // `devbox` job's header quotes `tool: cargo-nextest@` in prose. Renovate's
    // regexes skip the first and match the second only where a version follows,
    // so a false positive on either would make this check unusable on the very
    // file it exists for.
    let body = "jobs:\n  build:\n    steps:\n      \
                # the pins below are tracked by renovate.json's customManagers, which\n      \
                # match on `toolchain:` and `tool: cargo-nextest@` under .github/workflows\n      \
                - uses: dtolnay/rust-toolchain@v1\n        with:\n          toolchain: 1.97.1\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest@0.9.143\n      \
                - run: |\n          echo \"toolchain: $RESOLVED_TOOLCHAIN_DIR\"\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    assert!(
        out.status.success(),
        "a shell expansion and a prose mention must not be read as pins:\n{}",
        combined(&out)
    );
}

#[test]
fn yaml_extension_and_nested_dirs_match_renovates_file_set() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // renovate.json's managerFilePatterns are `.github/workflows/<name>.ya?ml`
    // — top level, either extension. This check must read the same set: a pin
    // it reads that Renovate does not, or the reverse, is a lockstep between
    // the wrong two things.
    let fixture = Fixture::new(
        &good_packages(),
        &[("ci.yaml", workflow("1.98.0", "0.9.143"))],
    );
    let out = fixture.run();
    assert!(
        !out.status.success(),
        "a `.yaml` workflow is in Renovate's set and must be in this one:\n{}",
        combined(&out)
    );

    let nested = fixture.dir.path().join(".github/workflows/nested");
    fs::create_dir_all(&nested).expect("create a nested workflow dir");
    fs::write(nested.join("ci.yml"), workflow("1.90.0", "0.9.1"))
        .expect("write the nested workflow");
    fs::write(
        fixture.dir.path().join(".github/workflows/ci.yaml"),
        workflow("1.97.1", "0.9.143"),
    )
    .expect("rewrite the top-level workflow");
    let out = fixture.run();
    assert!(
        out.status.success(),
        "a file one level down is outside Renovate's set, so reading it here \
         would fail on pins no bot maintains:\n{}",
        combined(&out)
    );
}
