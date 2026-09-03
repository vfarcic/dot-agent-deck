//! Issue #815: `scripts/devbox-check-gtk.sh` must reject a pkg-config answer
//! that resolves Tauri's GTK stack outside `/nix/store`.
//!
//! That property is a RUNTIME one — the script either compares the resolved
//! `libdir` against the store prefix or it does not, and no compile step can
//! tell. It is the same shape CLAUDE.md rule 5 records for
//! `xtask/linkage-check/src/clean_tmp.rs` and `junit_strip.rs`: a script whose
//! whole value is an assertion, which is exactly the class that stays green
//! while doing nothing.
//!
//! It is worth pinning here rather than trusting the script, because the thing
//! it replaced was ITSELF a check that looked right and asserted nothing
//! useful. `scripts/devbox-smoke.sh` used to resolve each module with
//! `pkg-config --modversion` and print the version — which succeeds identically
//! against the host's `/usr/lib/pkgconfig` and against the store, so the one
//! failure mode that actually shipped (#815) sailed through CI's `devbox` job.
//! Weakening the new assertion back to a presence check is a one-line edit that
//! restores that hole in full, and nothing else in the repository would notice.
//!
//! Four things are pinned:
//!
//! 1. a stubbed pkg-config answering with `/nix/store/…` libdirs PASSES, so the
//!    test is not merely asserting that the script always fails;
//! 2. the same stub answering `/usr/lib/x86_64-linux-gnu` — the literal value
//!    #815 measured on the host — FAILS, and says so naming the module;
//! 3. an EMPTY libdir fails too. `pkg-config --variable=libdir` on a `.pc` that
//!    does not define the variable exits 0 and prints nothing, so an empty
//!    answer is the one that a `case` on a prefix pattern most easily lets
//!    through;
//! 4. `scripts/devbox-smoke.sh` still invokes the script, because "inline that
//!    one-line helper back into the smoke test" is the edit that silently
//!    removes it from CI altogether.
//!
//! Tests only. The rule lives in the script; this is its gate. Linux-only, for
//! the same reason the script is: macOS builds Tauri against the system WebKit
//! and the flake yields an empty output there.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Workspace root, from this crate's manifest dir rather than the process cwd,
/// so the tests do not depend on how the runner was invoked.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/devbox-check-gtk.sh")
}

/// `bash` is on every GitHub runner and in this repo's devbox. Where it is
/// absent, say so loudly rather than failing a contributor's unrelated change —
/// the same discipline `verify_pr_stream.rs` and `junit_strip.rs` apply.
fn bash_present() -> bool {
    Command::new("bash")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Outcome of driving the real script against a stubbed `pkg-config`.
struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `scripts/devbox-check-gtk.sh` with a stand-in `pkg-config` first on
/// PATH that reports `libdir` as `libdir` for every module.
///
/// The stub answers `--modversion` and `--variable=libdir` and nothing else, so
/// a script that started consulting some third pkg-config flag would fail here
/// rather than silently reading through to the real tool. `None` means the
/// environment cannot run the script at all.
fn run_with_libdir(libdir: &str) -> Option<Run> {
    if !bash_present() {
        eprintln!("SKIP: devbox GTK origin test needs `bash` on PATH");
        return None;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");

    let stub = bin.join("pkg-config");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             \x20 --modversion) echo '1.2.3' ;;\n\
             \x20 --variable=libdir) printf '%s\\n' '{libdir}' ;;\n\
             \x20 *) echo \"stub pkg-config: unexpected argument $1\" >&2; exit 64 ;;\n\
             esac\n"
        ),
    )
    .expect("write pkg-config stand-in");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod pkg-config stand-in");
    }

    // The stub dir goes FIRST so it wins over any real pkg-config, but the rest
    // of PATH stays so the script still finds `uname` and `command`.
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(script())
        .env("PATH", path)
        .output()
        .expect("run devbox-check-gtk.sh");

    Some(Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// A store libdir is accepted. Without this the three negative cases below
/// would also pass against a script that unconditionally failed.
#[test]
fn devbox_gtk_origin_accepts_a_nix_store_libdir() {
    let Some(run) =
        run_with_libdir("/nix/store/fl089m700c7zxqwgly18jyx0fs4lgbbj-gtk+3-3.24.52/lib")
    else {
        return;
    };
    assert!(
        run.ok,
        "a /nix/store libdir must pass, else the check cannot be satisfied at all.\n\
         stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    assert!(
        run.stdout.contains("gtk+-3.0"),
        "the passing path should still report each module it resolved; stdout:\n{}",
        run.stdout
    );
}

/// The value #815 measured on the host. This is the regression that shipped.
#[test]
fn devbox_gtk_origin_rejects_the_host_distribution_libdir() {
    let Some(run) = run_with_libdir("/usr/lib/x86_64-linux-gnu") else {
        return;
    };
    assert!(
        !run.ok,
        "a /usr/lib libdir MUST fail — this is exactly #815's silent failure, \
         where every reporting step passed and only a test binary's loader \
         noticed. stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
    let combined = format!("{}{}", run.stdout, run.stderr);
    assert!(
        combined.contains("gtk+-3.0"),
        "the failure must name the offending module, or it is not actionable; \
         output:\n{combined}"
    );
    assert!(
        combined.contains("/usr/lib/x86_64-linux-gnu"),
        "the failure must quote what pkg-config actually answered; output:\n{combined}"
    );
}

/// `pkg-config --variable=libdir` exits 0 and prints nothing for a `.pc` that
/// does not define the variable, so "empty" is a real answer and not a stand-in
/// for an error.
#[test]
fn devbox_gtk_origin_rejects_an_empty_libdir() {
    let Some(run) = run_with_libdir("") else {
        return;
    };
    assert!(
        !run.ok,
        "an empty libdir must fail rather than glob-match the store prefix.\n\
         stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
}

/// A libdir that merely CONTAINS the store path must not pass. `/nix/store` has
/// to be a prefix, because a `-L` under `/usr` is what loses the rpath no
/// matter what else the string mentions.
#[test]
fn devbox_gtk_origin_requires_nix_store_as_a_prefix_not_a_substring() {
    let Some(run) = run_with_libdir("/usr/lib/x86_64-linux-gnu/nix/store/fake/lib") else {
        return;
    };
    assert!(
        !run.ok,
        "/nix/store must be matched as a prefix, not found anywhere in the string.\n\
         stdout:\n{}\nstderr:\n{}",
        run.stdout, run.stderr
    );
}

/// Does this shell script body actually RUN `devbox-check-gtk.sh`, as opposed to
/// merely mentioning it?
///
/// Comment lines are excluded deliberately, and that is the whole point of the
/// function existing rather than a `body.contains(…)` at the call site. The
/// `== tauri system libraries ==` section's explanatory comment names the script
/// too, so a substring test over the whole file stays green after the
/// invocation itself has been deleted — a guard that asserts nothing, which is
/// the exact shape of hole this file exists to close. Caught by Greptile on
/// PR #816 as a P2; [`comment_mention_alone_does_not_count`] pins it.
///
/// Matched loosely within a non-comment line rather than against the exact
/// current text, so reformatting the call (`"${0%/*}"` for `$(dirname "$0")`,
/// say) does not fail the build for no reason.
fn invokes_gtk_check(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .any(|line| line.contains("devbox-check-gtk.sh"))
}

/// The coupling that undoes all of the above in one line: `devbox-smoke.sh` is
/// what CI's `devbox` job actually runs, so a check it no longer invokes is a
/// check that runs nowhere.
#[test]
fn devbox_smoke_still_invokes_the_gtk_origin_check() {
    let smoke = repo_root().join("scripts/devbox-smoke.sh");
    let body = std::fs::read_to_string(&smoke).expect("read scripts/devbox-smoke.sh");
    assert!(
        invokes_gtk_check(&body),
        "scripts/devbox-smoke.sh must invoke scripts/devbox-check-gtk.sh on a \
         non-comment line — it is the only job in CI that can observe a devbox.json \
         GTK regression, so inlining or dropping the call removes the #815 guard \
         from CI entirely."
    );
}

/// The predicate above must not be satisfied by prose. Without this, the fix for
/// Greptile's P2 could itself regress to a substring test unnoticed.
#[test]
fn comment_mention_alone_does_not_count() {
    let commented_out = "\
echo '== tauri system libraries =='\n\
# devbox-check-gtk.sh asserts the resolved libdir is under /nix/store.\n\
#bash \"$(dirname \"$0\")/devbox-check-gtk.sh\"\n";
    assert!(
        !invokes_gtk_check(commented_out),
        "a commented-out call and a comment naming the script must both fail the \
         coupling check, or it is back to asserting nothing"
    );

    let real = "\
echo '== tauri system libraries =='\n\
# devbox-check-gtk.sh asserts the resolved libdir is under /nix/store.\n\
bash \"$(dirname \"$0\")/devbox-check-gtk.sh\"\n";
    assert!(
        invokes_gtk_check(real),
        "a real invocation alongside the comment must pass"
    );
}
