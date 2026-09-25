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
///
/// Every fixture also gets a `default-pnpm.yml` carrying an agreeing pnpm pin
/// ([`pnpm_workflow`] at [`PNPM`]) unless one of the workflows passed in already
/// names `pnpm/action-setup`. Without it every fixture would also fail the pnpm
/// class (issue #1262), and a test asserting only that the script FAILS — such
/// as `an_unreadable_pin_fails_even_when_every_readable_pin_agrees` — would
/// then pass for the wrong reason.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// `packages` are devbox.json entries verbatim, in the ARRAY form;
    /// `workflows` are `(file name, contents)` pairs written under
    /// `.github/workflows/`.
    fn new(packages: &[&str], workflows: &[(&str, String)]) -> Self {
        let quoted: Vec<String> = packages.iter().map(|p| format!("    \"{p}\"")).collect();
        Self::with_packages(&format!("[\n{}\n  ]", quoted.join(",\n")), workflows)
    }

    /// The same, with the whole `packages` VALUE written verbatim — so a test
    /// can pin the object form devbox itself writes (issue #791), which the
    /// `name@version` strings above cannot express.
    fn with_packages(packages: &str, workflows: &[(&str, String)]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir for the fixture repository");
        fs::write(
            dir.path().join("devbox.json"),
            format!("{{\n  \"packages\": {packages}\n}}\n"),
        )
        .expect("write the fixture devbox.json");

        let wf = dir.path().join(".github/workflows");
        fs::create_dir_all(&wf).expect("create the fixture workflow dir");
        for (name, body) in workflows {
            fs::write(wf.join(name), body).expect("write a fixture workflow");
        }
        // A full-line comment naming the action is not a step, and the
        // scanner ignores it too, so it must not suppress the default.
        if !workflows.iter().any(|(_, body)| {
            body.lines()
                .any(|l| !l.trim_start().starts_with('#') && l.contains("pnpm/action-setup"))
        }) {
            // A name no test passes, so the default can never overwrite a
            // workflow a test supplied (raised by Qodo on #1284).
            assert!(
                workflows
                    .iter()
                    .all(|(name, _)| *name != DEFAULT_PNPM_WORKFLOW),
                "{DEFAULT_PNPM_WORKFLOW} is reserved for the fixture's default pnpm pin"
            );
            fs::write(
                wf.join(DEFAULT_PNPM_WORKFLOW),
                pnpm_workflow(&format!("version: {PNPM}")),
            )
            .expect("write the fixture pnpm workflow");
        }
        Self { dir }
    }

    fn run(&self) -> Output {
        run_against(self.dir.path())
    }
}

/// The pnpm version the fixtures agree on.
const PNPM: &str = "11.22.0";

/// Where [`Fixture`] writes its default agreeing pnpm workflow.
const DEFAULT_PNPM_WORKFLOW: &str = "default-pnpm.yml";

/// devbox.json entries that agree with [`workflow`]'s defaults and [`PNPM`].
fn good_packages() -> Vec<&'static str> {
    vec![
        "jq@1.8.2",
        "cargo-nextest@0.9.143",
        "rustc@1.97.1",
        "cargo@1.97.1",
        "clippy@1.97.1",
        "rustfmt@1.97.1",
        "pnpm@11.22.0",
    ]
}

/// A workflow whose one `pnpm/action-setup` step carries `with_lines` verbatim
/// under its `with:` — `version: 11.22.0`, say — followed by a
/// `actions/setup-node` step, which is what follows it in the real files and
/// whose `node-version:` must not be read as a pnpm pin.
fn pnpm_workflow(with_lines: &str) -> String {
    format!(
        "jobs:\n  \
         desktop-web:\n    \
         steps:\n      \
         - uses: pnpm/action-setup@0977fd99725f1db4007ccb2928dbb4e90d06cc86 # v6\n        \
         with:\n          \
         {with_lines}\n      \
         - uses: actions/setup-node@v7\n        \
         with:\n          \
         node-version: 24\n"
    )
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
            "pnpm@11.22.0",
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

/// devbox.json's `packages` in the OBJECT form, with `rustc` carrying
/// per-package options — the exact shape `devbox add … --disable-plugin` leaves
/// behind, applied to a name this guard actually scans.
fn object_packages(nextest: &str, rustc: &str, clippy: &str) -> String {
    format!(
        "{{\n    \
         \"jq\": \"1.8.2\",\n    \
         \"cargo-nextest\": \"{nextest}\",\n    \
         \"rustc\": {{\n      \
         \"version\": \"{rustc}\",\n      \
         \"disable_plugin\": true\n    \
         }},\n    \
         \"cargo\": \"1.97.1\",\n    \
         \"clippy\": \"{clippy}\",\n    \
         \"rustfmt\": \"1.97.1\",\n    \
         \"pnpm\": \"11.22.0\",\n    \
         \"path:tauri-deps#tauri-deps\": \"\"\n  \
         }}"
    )
}

/// Issue #791: the object form is READ, in both its spellings.
///
/// This is not a hypothetical shape. `devbox add nodejs@24.12.0
/// --disable-plugin` rewrites the WHOLE `packages` block from the array form to
/// this one — every entry, not just the one that gained an option — so the day
/// any package needs a per-package option, every pin in the file changes
/// spelling at once. Renovate reads both forms (its devbox manager parses
/// `packages` as an array-or-record union), so both are pins it tracks and
/// bumps, and a guard that reads only one of them is a lockstep between the
/// wrong two things — the same hole as reading only block-style YAML (#710).
#[test]
fn object_form_packages_are_read() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::with_packages(
        &object_packages("0.9.143", "1.97.1", "1.97.1"),
        &good_workflows(),
    )
    .run();
    assert!(
        out.status.success(),
        "an agreeing object-form devbox.json must pass, or the two drift tests \
         below prove nothing:\n{}",
        combined(&out)
    );
}

/// The half that matters: drift is still caught through the object form. If it
/// were not, the conversion would have silently turned the guard off — which is
/// exactly what the array-only scanner did on the day devbox rewrote the file.
#[test]
fn drifted_object_form_nextest_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::with_packages(
        &object_packages("0.9.140", "1.97.1", "1.97.1"),
        &good_workflows(),
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "0.9.140 vs 0.9.143 must fail in the object form too:\n{text}"
    );
    assert!(
        text.contains("cargo-nextest") && text.contains("0.9.140") && text.contains("0.9.143"),
        "the failure must name the class and BOTH versions:\n{text}"
    );
}

/// A version living inside a per-package object is a pin like any other. This
/// is the one entry shape the array form cannot express at all, so without it
/// the `nodejs` entry that #791 created would be readable by nobody — and the
/// day someone puts `disable_plugin` on a Rust component, the guard would go
/// from comparing that pin to reporting it absent.
#[test]
fn a_version_nested_in_a_package_object_is_read() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::with_packages(
        &object_packages("0.9.143", "1.98.0", "1.97.1"),
        &good_workflows(),
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a nested `version` that disagrees must be compared, not skipped:\n{text}"
    );
    assert!(
        text.contains("internally inconsistent") && text.contains("1.98.0"),
        "the nested pin must be read as rustc's version and reported against its \
         three siblings:\n{text}"
    );
}

/// The per-name absence check survives the form change. Dropping `clippy` from
/// the object form must read as missing, not as agreeing by silence — the
/// Greptile finding that `one_missing_devbox_rust_component_fails` pins for the
/// array form.
#[test]
fn one_missing_devbox_rust_component_fails_in_object_form() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let packages = object_packages("0.9.143", "1.97.1", "1.97.1")
        .lines()
        .filter(|l| !l.contains("\"clippy\""))
        .collect::<Vec<_>>()
        .join("\n");
    let out = Fixture::with_packages(&packages, &good_workflows()).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "an object-form devbox.json missing one Rust component must fail:\n{text}"
    );
    assert!(
        text.contains("pins no clippy"),
        "the failure must name the component that went missing:\n{text}"
    );
}

/// The scan is scoped to the `packages` block, so a `shell.scripts` entry that
/// happens to share a package's name is not read as that package's pin.
///
/// Only reachable once object entries are read at all: `"cargo": "cargo build"`
/// is the same `"key": "value"` shape as a package, and the old array-form grep
/// could not have matched it. Getting this wrong fails LOUDLY (a script body is
/// not a version), but it would fail on a file whose pins are perfectly fine,
/// which makes the guard the problem.
#[test]
fn a_script_named_after_a_package_is_not_a_pin() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let fixture = Fixture::with_packages(
        &object_packages("0.9.143", "1.97.1", "1.97.1"),
        &good_workflows(),
    );
    let devbox = fixture.dir.path().join("devbox.json");
    let with_scripts = fs::read_to_string(&devbox)
        .expect("read the fixture devbox.json")
        .replace(
            "\n}\n",
            "\n,\n  \"shell\": {\n    \"scripts\": {\n      \
             \"cargo\": \"cargo build --release\",\n      \
             \"cargo-nextest\": \"cargo nextest run\"\n    }\n  }\n}\n",
        );
    fs::write(&devbox, with_scripts).expect("rewrite the fixture devbox.json");
    let out = fixture.run();
    assert!(
        out.status.success(),
        "a devbox SCRIPT named after a package must not be read as that package's \
         pin — the pins here agree and nothing should be reported:\n{}",
        combined(&out)
    );
}

/// Issue #1262, in the exact shape that shipped: a bare major. `pnpm/action-setup`
/// accepts `version: 12` and installs whatever 12.x is newest at run time, so
/// 12.6.0 reached `desktop-browser` with no commit here and hung it. The
/// fixture's devbox side is deliberately the SAME major, so this fails on
/// exactness and not merely because 12 differs from 11.22.0.
#[test]
fn a_major_only_pnpm_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let mut packages = good_packages();
    packages.retain(|p| !p.starts_with("pnpm@"));
    packages.push("pnpm@12.5.1");
    let out = Fixture::new(
        &packages,
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: 12")),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "`version: 12` floats across every 12.x release and must fail:\n{text}"
    );
    assert!(
        text.contains("not an exact X.Y.Z") && text.contains("desktop.yml"),
        "the failure must say the pin is not exact and point at the file:\n{text}"
    );
}

#[test]
fn drifted_pnpm_pin_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: 11.21.0")),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "11.22.0 vs 11.21.0 must fail:\n{text}"
    );
    assert!(
        text.contains("pnpm") && text.contains("11.21.0") && text.contains("11.22.0"),
        "the failure must name the class and BOTH versions:\n{text}"
    );
}

/// The half-applied bump: `desktop-web`, `desktop-browser` and the release's
/// desktop bundle each carry their own `pnpm/action-setup` step, so "all but
/// one moved" is the realistic way this pin goes wrong.
#[test]
fn one_drifted_pnpm_site_among_several_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: 11.22.0")),
            ("release.yml", pnpm_workflow("version: 11.27.1")),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "one pnpm site left behind must fail:\n{text}"
    );
    assert!(
        text.contains("internally inconsistent") && text.contains("release.yml"),
        "the failure must say the workflows disagree and point at the odd one:\n{text}"
    );
}

#[test]
fn a_pnpm_step_with_no_version_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("run_install: false")),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a pnpm/action-setup step with no version pins nothing and must fail:\n{text}"
    );
    assert!(
        text.contains("no version: input"),
        "the failure must say the version input is missing:\n{text}"
    );
}

/// The deliberate difference from the toolchain and nextest pins. Those are
/// read by regex customManagers that want a BARE X.Y.Z, so a quoted one is
/// untracked and rejected. This one is read by Renovate's github-actions
/// known-actions registry, which YAML-parses the step, so `"11.22.0"` is the same
/// tracked value as `11.22.0` — rejecting it would be a false positive. It must
/// still be COMPARED, which is the drift half.
#[test]
fn a_quoted_pnpm_pin_is_read_and_compared() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let agreeing = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: \"11.22.0\"")),
            ("release.yml", pnpm_workflow("version: '11.22.0'")),
        ],
    )
    .run();
    assert!(
        agreeing.status.success(),
        "a quoted pnpm pin is one Renovate reads, and here it agrees:\n{}",
        combined(&agreeing)
    );

    let drifted = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: \"11.21.0\"")),
        ],
    )
    .run();
    let text = combined(&drifted);
    assert!(
        !drifted.status.success() && text.contains("11.21.0") && !text.contains("not an exact"),
        "a quoted pin that drifted must be reported as a drift, not as unreadable:\n{text}"
    );
}

/// What makes a `version:` a pnpm pin is the step it sits in, so the scanner
/// walks the step rather than grepping for the key. This covers the three ways
/// that walk can go wrong: the inputs written BEFORE `uses:` in the step, in
/// flow style — and a `version:` on the NEXT step, which belongs to another
/// action and must not be read. The drifted value is on the pnpm step, so a
/// scanner that missed it would pass on the other step's agreeing value.
#[test]
fn a_pnpm_pin_is_found_by_its_step_not_by_its_key() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let body = "jobs:\n  desktop-web:\n    steps:\n      \
                - with: { version: 11.21.0, run_install: false }\n        \
                uses: pnpm/action-setup@v6\n      \
                - uses: some/other-action@v1\n        \
                with:\n          \
                version: 11.22.0\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", body.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the flow-style pin on the pnpm step has drifted and must be read:\n{text}"
    );
    assert!(
        text.contains("desktop.yml:4 11.21.0") && !text.contains("desktop.yml:7"),
        "exactly the pnpm step's version must be read, and the other action's \
         `version:` must not be:\n{text}"
    );
}

#[test]
fn devbox_without_pnpm_fails() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let packages: Vec<&str> = good_packages()
        .into_iter()
        .filter(|p| !p.starts_with("pnpm@"))
        .collect();
    let out = Fixture::new(&packages, &good_workflows()).run();
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "a workflow pnpm pin with nothing on the devbox side must fail:\n{text}"
    );
    assert!(
        text.contains("pins no pnpm"),
        "the failure must name the missing package:\n{text}"
    );
}

/// Raised by Qodo on #1284. Renovate YAML-parses `uses:` as it does `version:`,
/// so a quoted `uses: "pnpm/action-setup@…"` is the same tracked step. The
/// scanner used to match only the bare spelling, so a quoted site that drifted
/// was silently dropped and the class compared the survivors.
#[test]
fn a_quoted_pnpm_action_reference_is_still_a_site() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let body = "jobs:\n  desktop-web:\n    steps:\n      \
                - uses: \"pnpm/action-setup@v6\"\n        \
                with:\n          \
                version: 11.21.0\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", pnpm_workflow("version: 11.22.0")),
            ("release.yml", body.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("release.yml:6 11.21.0"),
        "the drifted pin behind a quoted `uses:` must be read and reported:\n{text}"
    );
}

/// Raised by Qodo on #1284. A trailing comment that mentions `version:` is not
/// an input, and reading it as one failed the guard on a file whose pin is
/// fine — the false positive that makes a guard the problem.
#[test]
fn a_version_in_a_trailing_comment_is_not_a_pnpm_pin() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            (
                "desktop.yml",
                pnpm_workflow(
                    "run_install: false # was version: 12\n          version: 11.22.0 # exact",
                ),
            ),
        ],
    )
    .run();
    assert!(
        out.status.success(),
        "a `version:` inside a trailing comment must not be read as a pin:\n{}",
        combined(&out)
    );
}

/// Raised by Greptile on #1284. Only the step's `with:` mapping carries the
/// action's input, so a `version:` elsewhere in the step — here an `env:` —
/// must not be read as the pin. It used to be, which let a step with NO pnpm
/// version pass the missing-pin check on an unrelated key's agreeing value.
#[test]
fn a_version_outside_the_with_mapping_is_not_a_pnpm_pin() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let body = "jobs:\n  desktop-web:\n    steps:\n      \
                - uses: pnpm/action-setup@v6\n        \
                env: { version: 11.22.0 }\n        \
                with:\n          \
                run_install: false\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", body.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("no version: input"),
        "an `env:` version is not the action's input, so this step is unpinned:\n{text}"
    );
}

/// Raised by Qodo on #1284. A flow mapping may span lines — `with: {` on one,
/// the keys on the next — and Renovate YAML-parses it like any other. The
/// scanner read a flow mapping only on the `with:` line itself, so this spelling
/// reported a correctly pinned step as having no version. The quoted
/// `${{ … }}` before the pin is the follow-up Qodo raised: a brace inside a
/// quoted value must not read as the mapping closing.
#[test]
fn a_multiline_flow_with_mapping_is_read() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let body = "jobs:\n  desktop-web:\n    steps:\n      \
                - uses: pnpm/action-setup@v6\n        \
                with: {\n          \
                package_json_file: \"${{ matrix.file }}\",\n          \
                version: 11.21.0,\n          \
                run_install: false }\n      \
                - uses: actions/setup-node@v7\n        \
                with:\n          \
                node-version: 24\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", body.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("desktop.yml:7 11.21.0"),
        "the version inside a multi-line flow mapping must be read and compared:\n{text}"
    );
    assert!(
        !text.contains("no version: input"),
        "a multi-line flow mapping carries the input, so the step is pinned:\n{text}"
    );
}

/// Raised by Qodo on #1284. A double-quoted scalar may span lines, so the
/// quote state has to survive the line break: here the `}` on the scalar's
/// second line is inside the quotes and must not close the mapping, which
/// would hide the drifted `version:` after it.
#[test]
fn a_quoted_scalar_spanning_lines_does_not_close_the_flow_mapping() {
    if !bash_present() {
        eprintln!("SKIP: needs `bash` on PATH");
        return;
    }
    let body = "jobs:\n  desktop-web:\n    steps:\n      \
                - uses: pnpm/action-setup@v6\n        \
                with: {\n          \
                package_json_file: \"first half\n            \
                second } half\",\n          \
                version: 11.21.0 }\n      \
                - uses: some/other-action@v1\n        \
                with:\n          \
                version: 11.22.0\n";
    let out = Fixture::new(
        &good_packages(),
        &[
            ("ci.yml", workflow("1.97.1", "0.9.143")),
            ("desktop.yml", body.to_string()),
        ],
    )
    .run();
    let text = combined(&out);
    assert!(
        !out.status.success() && text.contains("desktop.yml:8 11.21.0"),
        "the pin after a multi-line quoted scalar must be read:\n{text}"
    );
    assert!(
        !text.contains("desktop.yml:11"),
        "the next action's `version:` must never be read as this step's pin:\n{text}"
    );
}

/// Issue #451: the declared MSRV is a **third** copy of the pinned toolchain,
/// and it drifts for the same reason the other two did.
///
/// `Cargo.toml`'s `[workspace.package] rust-version` says what rustc this
/// project supports. It is set to the toolchain the project actually tests —
/// the one `devbox.json` installs and every workflow job asks
/// `dtolnay/rust-toolchain` for — because that is the only number anything
/// here verifies. A floor measured once and then left alone is a claim nothing
/// re-checks: no job builds on it, so the real minimum moves under it silently.
///
/// Which makes the declaration worth exactly as much as its agreement with the
/// pins, so this compares them. It reads the workflow side rather than
/// `devbox.json` deliberately: the workflow pins are a plain `toolchain: X.Y.Z`
/// this can parse in a few lines, and `repository_pins_are_in_lockstep` above
/// already ties those to devbox — so agreeing with one is agreeing with both,
/// without a second copy of `devbox_pins`' two-spelling parser.
///
/// Skips the `$VAR` sites for the same reason `scan_workflow_toolchain` does:
/// `windows-cross-check` echoes a resolved rustup directory, which is
/// diagnostics rather than a pin.
#[test]
fn the_declared_msrv_matches_the_pinned_toolchain() {
    let manifest = std::fs::read_to_string(repo_root().join("Cargo.toml"))
        .expect("the workspace manifest is readable");
    let declared = manifest
        .lines()
        .find_map(|l| {
            let rest = l.trim().strip_prefix("rust-version")?.trim_start();
            let value = rest.strip_prefix('=')?.trim().trim_matches('"');
            (!value.is_empty()).then(|| value.to_string())
        })
        .expect(
            "Cargo.toml declares `[workspace.package] rust-version` (issue #451). If it was \
             removed deliberately, this test goes with it — but read its doc comment first: \
             the point of the declaration is that it agrees with what CI installs.",
        );

    let mut sites: Vec<(String, String)> = Vec::new();
    let workflows = repo_root().join(".github/workflows");
    for entry in std::fs::read_dir(&workflows).expect("the workflow directory is readable") {
        let path = entry.expect("a readable directory entry").path();
        let is_yaml = path.extension().is_some_and(|e| e == "yml" || e == "yaml");
        if !is_yaml {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a readable workflow file");
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            let Some((_, after)) = trimmed.split_once("toolchain:") else {
                continue;
            };
            let value = after
                .trim()
                .split([' ', ',', '}'])
                .next()
                .unwrap_or_default()
                .to_string();
            if value.contains('$') || value.is_empty() {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            sites.push((name, value));
        }
    }

    assert!(
        !sites.is_empty(),
        "no `toolchain:` pin was found under .github/workflows/, so this check would pass \
         vacuously. Either the jobs stopped pinning a toolchain, or the spelling changed \
         (fix this test).",
    );

    let drifted: Vec<&(String, String)> = sites.iter().filter(|(_, v)| *v != declared).collect();
    assert!(
        drifted.is_empty(),
        "Cargo.toml declares rust-version = \"{declared}\" but {} workflow site(s) pin a \
         different toolchain: {}. The declared MSRV is the toolchain this project tests, so \
         the two move together — bump both, or the floor is a number nothing verifies \
         (issue #451).",
        drifted.len(),
        drifted
            .iter()
            .map(|(f, v)| format!("{f} => {v}"))
            .collect::<Vec<_>>()
            .join(", "),
    );
}
