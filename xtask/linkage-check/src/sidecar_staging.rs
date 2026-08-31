//! PRD #740: `desktop/scripts/prepare-sidecar.sh` stages the matching
//! `dot-agent-deck` daemon as a Tauri sidecar so a desktop bundle carries its
//! own daemon. It got the Windows filename wrong in **two** places, and both
//! were fatal:
//!
//! - the *source*: cargo emits `dot-agent-deck.exe` on a `*-pc-windows-*`
//!   target, so the `cp` failed on a missing file before anything was staged;
//! - the *destination*: Tauri resolves an `externalBin` entry as
//!   `{path}-{target_triple}{ext}` where `ext` is `".exe"` on Windows
//!   (`tauri-utils/src/resources.rs`), so a file staged without the suffix does
//!   not satisfy `externalBin` even once the source is found.
//!
//! Neither was reachable by any gate: nothing in `.github/workflows/` runs
//! `tauri build`, and no Windows bundle has ever been cut. The fix therefore
//! shipped ahead of the artifact it unblocks (#740 defers Windows bundling
//! behind #741), which makes these tests the only thing standing between the
//! two-line rule and a silent regression.
//!
//! Every case drives the **real** script under a stubbed `cargo` inside a
//! `tempfile::tempdir()`, so nothing is asserted about a paraphrase of the
//! rule. The stub emits exactly what cargo would emit for the requested target
//! — `.exe` on Windows triples, bare elsewhere — which is what lets the Windows
//! case fail for the reporter's reason rather than for a fixture mistake.
//!
//! Unix-only, matching `pin_lockstep`: the script needs a POSIX shell, and
//! making a Git-Bash path translation the difference between a green and a red
//! `build-windows` buys nothing.

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

fn real_script() -> PathBuf {
    repo_root().join("desktop/scripts/prepare-sidecar.sh")
}

/// Same shape as `pin_lockstep`'s probe: say so loudly rather than failing a
/// contributor's unrelated change on a missing interpreter.
fn sh_present() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A stub `cargo` that creates the artifact cargo really would, for whichever
/// `--target` and profile it is asked for — and nothing else. Keyed to the one
/// invocation under test so it cannot accidentally satisfy some other path.
const CARGO_STUB: &str = r#"#!/bin/sh
set -eu
manifest=""
triple=""
profile=debug
saw_build=0
while [ $# -gt 0 ]; do
  case "$1" in
    build) saw_build=1; shift ;;
    --manifest-path) manifest=$2; shift 2 ;;
    --target) triple=$2; shift 2 ;;
    --release) profile=release; shift ;;
    *) shift ;;
  esac
done
if [ "$saw_build" -ne 1 ] || [ -z "$manifest" ] || [ -z "$triple" ]; then
  echo "stub cargo: unexpected invocation" >&2
  exit 97
fi
root=$(dirname "$manifest")
case "$triple" in
  *-pc-windows-*) ext=.exe ;;
  *) ext= ;;
esac
out="$root/target/$triple/$profile"
mkdir -p "$out"
printf 'stub daemon\n' > "$out/dot-agent-deck$ext"
chmod 755 "$out/dot-agent-deck$ext"
"#;

/// A throwaway repository whose only real content is the script under test.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join("desktop/scripts")).expect("script dir");
        fs::copy(
            real_script(),
            root.join("desktop/scripts/prepare-sidecar.sh"),
        )
        .expect("copy the real script under test");

        // `prepare-sidecar.sh` passes this to cargo; the stub reads the repo
        // root back out of it, so it has to exist as a path even though no
        // cargo ever parses it here.
        fs::write(root.join("Cargo.toml"), "# stub\n").expect("manifest");

        fs::create_dir_all(root.join("bin")).expect("bin dir");
        let cargo = root.join("bin/cargo");
        fs::write(&cargo, CARGO_STUB).expect("cargo stub");
        make_executable(&cargo);

        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Shadow `rustc` with one that resolves no host triple, so the
    /// unresolvable-triple case is deterministic instead of depending on
    /// whether a real toolchain happens to sit on the system PATH.
    fn shadow_rustc_with_no_host(&self) {
        let rustc = self.root().join("bin/rustc");
        fs::write(&rustc, "#!/bin/sh\nexit 0\n").expect("rustc stub");
        make_executable(&rustc);
    }

    /// Run the script with `TAURI_ENV_TARGET_TRIPLE` set, which is how Tauri
    /// itself invokes it during a cross-target bundle.
    fn run(&self, triple: &str, profile: &str) -> Output {
        self.run_with_triple_env(Some(triple), profile)
    }

    fn run_with_triple_env(&self, triple: Option<&str>, profile: &str) -> Output {
        let root = self.root();
        // `sh` by absolute path, because PATH below is rewritten for the child
        // and resolving the interpreter through it would only test the fixture.
        let mut cmd = Command::new("/bin/sh");
        // The stub bin dir goes FIRST so it shadows any real cargo, while the
        // system coreutils the script genuinely needs (mkdir, cp, chmod,
        // dirname, sed) stay reachable. Shadowing rather than emptying is what
        // keeps a failing case from being rescued by the developer's toolchain.
        let path = format!("{}:/usr/bin:/bin", root.join("bin").display());
        cmd.arg(root.join("desktop/scripts/prepare-sidecar.sh"))
            .arg(profile)
            .env("PATH", path)
            .current_dir(root);
        match triple {
            Some(t) => {
                cmd.env("TAURI_ENV_TARGET_TRIPLE", t);
            }
            None => {
                cmd.env_remove("TAURI_ENV_TARGET_TRIPLE");
            }
        }
        cmd.output().expect("run prepare-sidecar.sh")
    }

    fn staged(&self, name: &str) -> PathBuf {
        self.root().join("desktop/src-tauri/binaries").join(name)
    }

    /// Every filename actually present in the staging directory, so a failure
    /// message can say what WAS produced rather than only what was missing.
    fn staged_names(&self) -> Vec<String> {
        let dir = self.root().join("desktop/src-tauri/binaries");
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).expect("stat stub").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod stub");
}

fn combined(out: &Output) -> String {
    format!(
        "status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn windows_triple_stages_the_sidecar_with_an_exe_suffix() {
    if !sh_present() {
        eprintln!("SKIP: prepare-sidecar.sh needs a POSIX `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let triple = "x86_64-pc-windows-msvc";
    let out = fx.run(triple, "release");

    assert!(
        out.status.success(),
        "staging a Windows sidecar must succeed: cargo emits \
         `dot-agent-deck.exe` on a *-pc-windows-* target, so a script that \
         copies `dot-agent-deck` fails on a missing source file:\n{}",
        combined(&out)
    );
    let expected = fx.staged(&format!("dot-agent-deck-{triple}.exe"));
    assert!(
        expected.is_file(),
        "Tauri resolves an externalBin entry as `{{path}}-{{triple}}{{ext}}` \
         with ext=`.exe` on Windows, so the staged file must carry the suffix. \
         Staged instead: {:?}\n{}",
        fx.staged_names(),
        combined(&out)
    );
}

#[test]
fn aarch64_windows_triple_also_gets_the_suffix() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let triple = "aarch64-pc-windows-msvc";
    let out = fx.run(triple, "release");

    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        fx.staged(&format!("dot-agent-deck-{triple}.exe")).is_file(),
        "the rule is the `*-pc-windows-*` family, not one hardcoded triple. \
         Staged: {:?}\n{}",
        fx.staged_names(),
        combined(&out)
    );
}

/// The control: the same script, the same stub, one character of difference in
/// the triple. Without it, a test that only ever saw Windows could pass by
/// appending `.exe` unconditionally.
#[test]
fn unix_triple_stages_the_sidecar_with_no_suffix() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let triple = "x86_64-unknown-linux-gnu";
    let out = fx.run(triple, "release");

    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        fx.staged(&format!("dot-agent-deck-{triple}")).is_file(),
        "a non-Windows triple must stage a bare name. Staged: {:?}\n{}",
        fx.staged_names(),
        combined(&out)
    );
    assert!(
        !fx.staged(&format!("dot-agent-deck-{triple}.exe")).is_file(),
        "a non-Windows triple must NOT gain an `.exe`. Staged: {:?}",
        fx.staged_names()
    );
}

#[test]
fn darwin_triple_stages_the_sidecar_with_no_suffix() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let triple = "aarch64-apple-darwin";
    let out = fx.run(triple, "release");

    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        fx.staged(&format!("dot-agent-deck-{triple}")).is_file(),
        "Staged: {:?}\n{}",
        fx.staged_names(),
        combined(&out)
    );
}

#[test]
fn debug_profile_stages_from_the_debug_artifact_dir() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let triple = "x86_64-pc-windows-msvc";
    let out = fx.run(triple, "debug");

    assert!(out.status.success(), "{}", combined(&out));
    assert!(
        fx.root()
            .join(format!("target/{triple}/debug/dot-agent-deck.exe"))
            .is_file(),
        "the debug profile must build into target/<triple>/debug, or the \
         profile argument is not reaching cargo:\n{}",
        combined(&out)
    );
    assert!(
        fx.staged(&format!("dot-agent-deck-{triple}.exe")).is_file(),
        "the `.exe` rule is independent of the profile. Staged: {:?}",
        fx.staged_names()
    );
}

#[test]
fn an_unresolvable_target_triple_is_a_hard_error() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    // `rustc` resolves no host and there is no env override, so the triple
    // cannot be determined. It must fail loudly rather than stage something
    // under an empty triple that would never satisfy `externalBin`.
    fx.shadow_rustc_with_no_host();
    let out = fx.run_with_triple_env(None, "release");

    assert!(
        !out.status.success(),
        "an unresolvable target triple must fail the script:\n{}",
        combined(&out)
    );
    assert!(
        fx.staged_names().is_empty(),
        "nothing may be staged when the triple is unknown, got {:?}",
        fx.staged_names()
    );
}

#[test]
fn an_unknown_profile_is_rejected() {
    if !sh_present() {
        eprintln!("SKIP: needs `sh` on PATH");
        return;
    }
    let fx = Fixture::new();
    let out = fx.run("x86_64-unknown-linux-gnu", "profitable");

    assert_eq!(
        out.status.code(),
        Some(2),
        "an unrecognised profile must exit 2 with a usage message:\n{}",
        combined(&out)
    );
    assert!(
        fx.staged_names().is_empty(),
        "nothing may be staged for an unknown profile, got {:?}",
        fx.staged_names()
    );
}
