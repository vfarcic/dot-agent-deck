//! Issue #250 — unit tests for the build-time `DAD_VERSION` / `DAD_BUILD_ID`
//! resolver.
//!
//! `cargo test` cannot reach code inside `build.rs`: the build script is
//! compiled as its own binary and this crate cannot import from it. So the
//! resolution logic lives in `build_version_resolve.rs` at the repo root,
//! which `build.rs` and this file both compile as an out-of-line module (a
//! module rather than an `include!` so rustfmt — which does not follow
//! `include!` — covers the shared file too). The functions are pure — they
//! take the env value and the git output as parameters — so these tests never
//! mutate the process environment and never shell out to git.

#[path = "../build_version_resolve.rs"]
mod build_version_resolve;

use build_version_resolve::{
    VersionSource, escape_for_cargo_warning, is_single_line_directive_value, normalize_build_id,
    normalize_version, resolve_build_id, resolve_version,
};

/// The `Cargo.toml` placeholder, i.e. what a source build wrongly reported
/// before this fix. Spelled out rather than read from `CARGO_PKG_VERSION` so
/// the expectations stay readable.
const PLACEHOLDER: &str = "0.1.0";

/// What a build reports when it falls through to [`PLACEHOLDER`]: the
/// placeholder marked as a build whose version is unknown (issue #326).
const PLACEHOLDER_REPORTED: &str = "0.1.0+unknown";

#[test]
fn version_resolution_prefers_injection_then_git_then_cargo_pkg_version() {
    // 1. A valid injected DAD_VERSION wins even when git has a usable tag.
    let r = resolve_version(Some("2.3.4"), Some("v1.0.0"), PLACEHOLDER);
    assert_eq!(r.value, "2.3.4");
    assert_eq!(r.source, VersionSource::Injected);

    // The injected value goes through the same `v`/`V` strip and whitespace
    // trim as a git tag.
    assert_eq!(
        resolve_version(Some(" v2.3.4\n"), Some("v1.0.0"), PLACEHOLDER).value,
        "2.3.4"
    );
    assert_eq!(
        resolve_version(Some("1.2.3-alpha.0"), None, PLACEHOLDER).value,
        "1.2.3-alpha.0",
        "a SemVer pre-release suffix is a legitimate injection (this project tags them)"
    );

    // 2. No injection -> the git tag, exactly as before this change.
    let r = resolve_version(None, Some("v1.0.0"), PLACEHOLDER);
    assert_eq!(r.value, "1.0.0");
    assert_eq!(r.source, VersionSource::Git);

    // An empty / blank injection is "not injected", not an empty version.
    let r = resolve_version(Some("   "), Some("v1.0.0"), PLACEHOLDER);
    assert_eq!(r.value, "1.0.0");
    assert_eq!(r.source, VersionSource::Git);

    // 3. Neither -> the CARGO_PKG_VERSION placeholder (build.rs warns here).
    let r = resolve_version(None, None, PLACEHOLDER);
    assert_eq!(r.value, PLACEHOLDER_REPORTED);
    assert_eq!(r.source, VersionSource::Placeholder);
}

#[test]
fn build_id_prefers_injection_then_composes_then_degrades_to_unknown() {
    // 1. An injected DAD_BUILD_ID wins, even with git metadata available — a
    //    packager's id has no format to conform to beyond the bounded
    //    single-line alphabet.
    assert_eq!(
        resolve_build_id(Some("1.2.3-nixpkgs"), "1.2.3", Some("abc1234"), true),
        "1.2.3-nixpkgs"
    );
    assert_eq!(
        resolve_build_id(Some("  1.2.3-nixpkgs  "), "1.2.3", None, false),
        "1.2.3-nixpkgs",
        "surrounding whitespace from a shell export is trimmed"
    );

    // 2. No injection -> `<version>-g<short-sha>[-dirty]` (PRD #103 M1.0).
    assert_eq!(
        resolve_build_id(None, "1.2.3", Some("abc1234"), false),
        "1.2.3-gabc1234"
    );
    assert_eq!(
        resolve_build_id(None, "1.2.3", Some("abc1234"), true),
        "1.2.3-gabc1234-dirty"
    );

    // 3. No injection and no git metadata -> `<version>-unknown`, today's
    //    degradation, pinned. The dirty flag cannot reach the composed id.
    assert_eq!(
        resolve_build_id(None, "1.2.3", None, false),
        "1.2.3-unknown"
    );
    assert_eq!(resolve_build_id(None, "1.2.3", None, true), "1.2.3-unknown");
    assert_eq!(
        resolve_build_id(Some(""), "1.2.3", Some(""), false),
        "1.2.3-unknown",
        "blank strings count as absent on both inputs"
    );
}

#[test]
fn invalid_injected_version_falls_through_instead_of_being_emitted() {
    // `src/version.rs` parses DAD_VERSION with `semver` and `.expect()`s, so
    // emitting any of these would panic the binary at startup. Each must fall
    // through to the next resolution step instead.
    for bad in [
        "1.2",            // not a X.Y.Z core
        "1.2.3.4",        // too many core fields
        "release-7",      // not numeric at all
        "v",              // bare prefix
        "",               // blank
        "1.2.x",          // non-numeric core field
        "01.2.3",         // SemVer forbids a leading zero in a core field
        "1.2.3-",         // empty pre-release
        "1.2.3-alpha..1", // empty pre-release identifier
        "1.2.3-alpha_1",  // `_` is not in the pre-release alphabet
        "1.2.3-01",       // leading zero in a numeric pre-release identifier
        "1.2.3 4.5.6",    // interior whitespace, not one version
        // A core field wider than the `u64` each is parsed into.
        "18446744073709551616.0.0",
    ] {
        let r = resolve_version(Some(bad), Some("v3.4.5"), PLACEHOLDER);
        assert_eq!(
            (r.value.as_str(), r.source),
            ("3.4.5", VersionSource::Git),
            "invalid injected DAD_VERSION {bad:?} must fall through to the git tag"
        );

        let r = resolve_version(Some(bad), None, PLACEHOLDER);
        assert_eq!(
            (r.value.as_str(), r.source),
            (PLACEHOLDER_REPORTED, VersionSource::Placeholder),
            "invalid injected DAD_VERSION {bad:?} with no git tag must fall through to the \
             CARGO_PKG_VERSION placeholder"
        );
    }

    // The same validation guards the git path, so a junk tag cannot be
    // emitted either.
    let r = resolve_version(None, Some("nightly"), PLACEHOLDER);
    assert_eq!(
        (r.value.as_str(), r.source),
        (PLACEHOLDER_REPORTED, VersionSource::Placeholder)
    );

    // Everything the resolver *does* accept must parse under the same
    // `semver` parser `src/version.rs` uses — and, symmetrically, everything
    // that parser accepts must be accepted here. A resolver stricter than the
    // parser is not "safe": it sends a *correctly* injected packager build
    // down the `0.1.0` placeholder path, which is the exact bug #250 fixes.
    // The last three cases are the ones a hand-rolled grammar got wrong:
    // SemVer build metadata, and a numeric pre-release wider than `u64`
    // (legal — a pre-release identifier is a string, not a number).
    for good in [
        "1.2.3",
        "v0.35.2",
        "0.25.0-alpha.0",
        "10.0.0-rc.1",
        "0.0.0",
        "1.2.3+meta",
        "1.2.3-alpha+meta",
        "1.2.3-99999999999999999999999",
    ] {
        let value =
            normalize_version(good).unwrap_or_else(|| panic!("{good:?} should be accepted"));
        semver::Version::parse(&value)
            .unwrap_or_else(|e| panic!("accepted {good:?} -> {value:?} must parse as semver: {e}"));
        assert_eq!(
            resolve_version(Some(good), None, PLACEHOLDER).source,
            VersionSource::Injected,
            "{good:?} is valid SemVer, so injecting it must win rather than degrade to the \
             placeholder"
        );
    }
}

/// FIX 1 (reviewer #1 / auditor #1): Cargo parses build-script stdout **line by
/// line**, so an interior newline in an emitted value promotes the rest of the
/// value into a second `cargo:` directive. `DAD_BUILD_ID='foo\ncargo:rustc-env=
/// INJECTED=yes'` used to set both variables.
#[test]
fn injected_build_id_with_a_line_break_cannot_inject_a_cargo_directive() {
    for hostile in [
        "foo\ncargo:rustc-env=INJECTED=yes",
        "foo\r\ncargo:rustc-cfg=feature=\"backdoor\"",
        "foo\rcargo:rustc-link-arg=-Wl,--wrap=main",
        "foo\ncargo:warning=spoofed",
        "1.2.3-g0000000\nrustc-env=X=y",
        "foo\u{1b}[2Jbar",     // terminal escape, no line break
        "foo\u{202e}rab",      // bidi override, no line break
        "two words",           // interior whitespace is not trimmed away
        "id\u{7f}",            // DEL
        "cargo:rustc-env=X=y", // inert on its own, but not a build id
        "\n",                  // whitespace-only after the newline
        "1.2.3;rm -rf /",      // outside the bounded alphabet
    ] {
        assert_eq!(
            normalize_build_id(hostile),
            None,
            "{hostile:?} must not be accepted as a build id"
        );
        // Rejection is a fall-through, exactly as if the variable were absent:
        // the composed git value, then `-unknown`.
        assert_eq!(
            resolve_build_id(Some(hostile), "1.2.3", Some("abc1234"), false),
            "1.2.3-gabc1234",
            "a rejected build id must fall through to the composed value"
        );
        assert_eq!(
            resolve_build_id(Some(hostile), "1.2.3", None, false),
            "1.2.3-unknown",
            "a rejected build id with no git metadata must fall through to `-unknown`"
        );
    }

    // The bounded alphabet still accepts every id a real packager writes.
    for ok in [
        "1.2.3-gabc1234",
        "1.2.3-gabc1234-dirty",
        "1.2.3-nixpkgs",
        "1.2.3-1ubuntu1",
        "1.2.3+build.7",
        "0.35.2_rc1",
    ] {
        assert_eq!(normalize_build_id(ok).as_deref(), Some(ok));
    }
}

/// FIX 1, version side: an injected `DAD_VERSION` carrying a newline must be
/// rejected (it is not valid SemVer) **and** must be safe to name in the
/// `cargo:warning=` that reports the rejection — `build.rs` renders it with
/// [`escape_for_cargo_warning`] rather than interpolating it raw, so the
/// rejected value cannot open a directive line of its own.
#[test]
fn rejected_version_is_safe_to_name_in_a_cargo_warning() {
    let hostile = "1.2\ncargo:rustc-env=INJECTED=yes";
    assert_eq!(normalize_version(hostile), None);
    assert_eq!(
        resolve_version(Some(hostile), Some("v3.4.5"), PLACEHOLDER).source,
        VersionSource::Git
    );

    let escaped = escape_for_cargo_warning(hostile);
    assert!(
        is_single_line_directive_value(&escaped),
        "the escaped form must be a single safe line, got {escaped:?}"
    );
    assert!(
        !escaped.contains('\n') && !escaped.contains('\r'),
        "the escaped form must not carry a real line break, got {escaped:?}"
    );
    assert!(
        escaped.contains("\\ncargo:rustc-env=INJECTED=yes"),
        "the value must still be legible to whoever reads the build log, got {escaped:?}"
    );

    // Control and bidi codepoints are escaped too, so a warning cannot clear
    // or reorder the terminal reading it.
    assert_eq!(escape_for_cargo_warning("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
    assert_eq!(escape_for_cargo_warning("a\u{202e}b"), "a\\u{202e}b");
    assert_eq!(
        escape_for_cargo_warning("plain-1.2.3"),
        "plain-1.2.3",
        "an ordinary value is left alone"
    );
}

/// The line-protocol predicate every emitted value passes through in
/// `build.rs`'s `emit_rustc_env`, pinned directly.
#[test]
fn line_protocol_check_rejects_control_characters() {
    assert!(is_single_line_directive_value("1.2.3-gabc1234"));
    assert!(is_single_line_directive_value("1.2.3+meta"));
    assert!(!is_single_line_directive_value("a\nb"));
    assert!(!is_single_line_directive_value("a\rb"));
    assert!(!is_single_line_directive_value("a\u{1b}b"));
    assert!(!is_single_line_directive_value("a\u{85}b"));
    assert!(!is_single_line_directive_value("a\u{0}b"));

    // Everything the resolver can emit passes it, on every path.
    for value in [
        resolve_version(Some("v1.2.3+meta"), None, PLACEHOLDER).value,
        resolve_version(None, Some("v0.35.2"), PLACEHOLDER).value,
        resolve_version(None, None, PLACEHOLDER).value,
        resolve_build_id(Some("1.2.3-nixpkgs"), "1.2.3", None, false),
        resolve_build_id(None, "1.2.3", Some("abc1234"), true),
        resolve_build_id(None, "1.2.3", None, false),
    ] {
        assert!(
            is_single_line_directive_value(&value),
            "{value:?} must be safe to emit as a cargo: directive value"
        );
    }
}
