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

/// The same two pins in YAML's OTHER spelling. A flow mapping (`with: { … }`)
/// means exactly what the block form means, and renovate.json's unanchored
/// regexes read it — but the toolchain scanner used to anchor at the start of
/// the line, so this whole spelling was invisible to the guard (issue #710).
fn flow_workflow(toolchain: &str, nextest: &str) -> String {
    format!(
        "jobs:\n  \
         build:\n    \
         steps:\n      \
         - uses: dtolnay/rust-toolchain@v1\n        \
         with: {{ toolchain: {toolchain} }}\n      \
         - uses: taiki-e/install-action@v2\n        \
         with: {{tool: cargo-nextest@{nextest}}}\n"
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
    // regex over a bare X.Y.Z, so a value it cannot parse stops being tracked
    // without anything going red. Here it goes red. Note what this fixture
    // does NOT cover — `1.97.x` is not a version in any spelling, so it would
    // fail even if the quotes were ignored. The quoting itself is covered by
    // `a_quoted_but_otherwise_valid_toolchain_pin_fails` below.
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
fn a_quoted_but_otherwise_valid_toolchain_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The dangerous half of the case above, and the one a YAML formatter
    // actually produces: the quoted value is a PERFECTLY GOOD version, and
    // agrees with devbox.json. YAML gives `"1.97.1"` and `1.97.1` the same
    // meaning, so nothing looks wrong — but renovate.json matches a bare
    // X.Y.Z, so the quotes end the tracking silently. The script used to strip
    // one layer of quotes before testing the value, which normalised this into
    // a pass; the sibling test above only failed because `1.97.x` is not a
    // version in any spelling, so it never covered this.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: \"1.97.1\"\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest@0.9.143\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a quoted toolchain pin is invisible to Renovate and must fail even \
         though its value agrees with devbox.json:\n{text}"
    );
    assert!(
        text.contains("unreadable"),
        "the failure must name the pin as unreadable:\n{text}"
    );
}

#[test]
fn a_quoted_but_otherwise_valid_nextest_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Same silent-rot class on the other pin. renovate.json wants
    // `tool:` followed DIRECTLY by a bare `cargo-nextest@X.Y.Z`, so quoting the
    // whole scalar stops it being tracked while leaving the version readable to
    // any YAML parser — and to an earlier version of this script, which scanned
    // the line for the token rather than checking Renovate could reach it.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with:\n          toolchain: 1.97.1\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: \"cargo-nextest@0.9.143\"\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a quoted cargo-nextest pin is invisible to Renovate and must fail \
         even though its value agrees with devbox.json:\n{text}"
    );
    assert!(
        text.contains("cannot read"),
        "the failure must say renovate.json cannot read the pin:\n{text}"
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
fn one_missing_devbox_rust_component_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Raised by Greptile on the PR, and it reproduced exactly: drop `clippy`
    // and the guard reported `ok` twice and exited 0. The three survivors still
    // agreed with each other and with the workflows, and `compare` only ever
    // sees versions that were FOUND — so an absent component was indistinguishable
    // from one that matches. `a_side_with_no_pins_at_all_fails` below covers the
    // whole class vanishing; this covers one of four, which is the reachable
    // case, since a nixpkgs rename moves one package at a time.
    let packages: Vec<&str> = good_packages()
        .into_iter()
        .filter(|p| !p.starts_with("clippy@"))
        .collect();
    let out = Fixture::new(&packages, &good_workflows()).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "devbox.json pinning only three of the four Rust components must fail, \
         not pass on the agreement of the survivors:\n{text}"
    );
    assert!(
        text.contains("pins no clippy"),
        "the failure must name the component that went missing:\n{text}"
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
    //
    // This became load-bearing for the TOOLCHAIN half only in #710. Until the
    // scanner stopped anchoring at the start of the line, neither the `echo`
    // nor the comment could reach it — both put the token mid-line — so the
    // `$`-expansion skip and the comment exclusion were exercised on the
    // nextest side alone. Un-anchoring is precisely what puts these two lines
    // in front of the toolchain scanner, and they are the reason #710 was not
    // a one-line regex swap.
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

#[test]
fn a_drifted_flow_style_toolchain_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // Issue #710, in the exact shape that made it a hole rather than a wart:
    // one block-style site agreeing with devbox.json, one flow-style site that
    // has drifted. The scanner anchored its grep at `^[[:space:]]*toolchain:`,
    // so the flow site was not a site at all — the class compared the one pin
    // it could see, found it agreeing, and printed `ok` while a pin Renovate
    // tracks and bumps sat a whole minor release away. Verified against the
    // pre-fix script: it exited 0.
    let drifted = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                   with: { toolchain: 1.98.0 }\n      \
                   - uses: taiki-e/install-action@v2\n        with:\n          \
                   tool: cargo-nextest@0.9.143\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("release.yml", drifted.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a flow-style `with: {{ toolchain: X.Y.Z }}` is a pin Renovate bumps, \
         so its drift must fail here rather than pass unseen:\n{text}"
    );
    assert!(
        text.contains("release.yml") && text.contains("1.98.0"),
        "the failure must point at the flow-style file and name its version:\n{text}"
    );
    assert!(
        !text.contains("unreadable"),
        "flow style is ACCEPTED, not reported: calling a perfectly readable \
         pin unreadable would be the opposite false positive:\n{text}"
    );
}

#[test]
fn a_drifted_flow_style_nextest_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The sibling scanner reached the line — it already matched its token
    // anywhere on a non-comment line — but not the VALUE: its token class
    // ran to the closing brace, so `{tool: cargo-nextest@0.9.140}` yielded
    // `0.9.140}`, failed the semver test, and was reported as an unreadable
    // pin. Same class of wrong answer as the toolchain half, arrived at from
    // the other side, and fixed in the same change so the two stay consistent.
    let drifted = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                   with:\n          toolchain: 1.97.1\n      \
                   - uses: taiki-e/install-action@v2\n        \
                   with: {tool: cargo-nextest@0.9.140}\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("release.yml", drifted.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a flow-style cargo-nextest pin that has drifted must fail:\n{text}"
    );
    assert!(
        text.contains("release.yml") && text.contains("0.9.140"),
        "the failure must point at the flow-style file and name its version:\n{text}"
    );
    assert!(
        !text.contains("unreadable"),
        "the drift must be reported as a drift, not mistaken for an \
         unparseable value:\n{text}"
    );
}

#[test]
fn agreeing_flow_style_pins_pass() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // The deliberate decision recorded in the script: a flow-style pin is
    // ACCEPTED and compared rather than reported as unreadable. It has to be —
    // Renovate reads it, so it is tracked, so it can drift, and a guard that
    // refuses to read what Renovate reads is a lockstep between the wrong two
    // things. This is the half that would go red if someone "fixed" the value
    // extraction by widening it back to the rest of the line: `1.97.1 }` is
    // not a version.
    let out = Fixture::new(
        &good_packages(),
        &[("ci.yml", flow_workflow("1.97.1", "0.9.143"))],
    )
    .run();
    assert!(
        out.status.success(),
        "flow style is the same mapping as block style and agrees with \
         devbox.json here, so it must pass:\n{}",
        combined(&out)
    );
}

#[test]
fn a_quoted_flow_style_toolchain_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    // #707's rule survives the #710 rework in the spelling that could most
    // easily have broken it. Reading flow style means stopping the value at a
    // `,` or a `}`, and a careless stop would also swallow the quotes and hand
    // back a valid-looking 1.97.1 — turning the silent-rot case back into a
    // pass. The quotes have to reach the semver test intact here exactly as
    // they do in block style, so the extra key after the comma is deliberate.
    let body = "jobs:\n  build:\n    steps:\n      - uses: dtolnay/rust-toolchain@v1\n        \
                with: { toolchain: \"1.97.1\", components: clippy }\n      \
                - uses: taiki-e/install-action@v2\n        with:\n          \
                tool: cargo-nextest@0.9.143\n";
    let out = Fixture::new(&good_packages(), &[("ci.yml", body.to_string())]).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a quoted pin is invisible to Renovate in either YAML spelling:\n{text}"
    );
    assert!(
        text.contains("unreadable"),
        "the failure must name the pin as unreadable, not merely absent:\n{text}"
    );
}
