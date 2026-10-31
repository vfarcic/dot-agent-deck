// Pure resolution logic for the build-time `DAD_VERSION` / `DAD_BUILD_ID`
// values (issue #250).
//
// This file is compiled into two unrelated compilation units as an
// **out-of-line module**:
//
// - `build.rs` declares `mod build_version_resolve;` and supplies the real
//   inputs (the process environment and `git` subprocess output) plus the
//   `cargo:` emission; and
// - `tests/build_version.rs` declares
//   `#[path = "../build_version_resolve.rs"] mod build_version_resolve;` and
//   exercises the same functions with hand-written inputs.
//
// The split exists because `cargo test` cannot reach code that lives inside a
// build script: `build.rs` is compiled as its own binary and the test crate
// cannot import from it. So everything worth testing lives here, as pure
// functions taking their inputs as parameters, and everything impure (running
// `git`, reading the process environment, `println!("cargo:…")`) stays in
// `build.rs`.
//
// A `mod` rather than an `include!` on purpose: rustfmt does not follow
// `include!`, so the mandatory `cargo fmt --check` gate used to stay green over
// an unformatted copy of this file. A module declaration (including the
// `#[path]` form) *is* followed, so this file is now covered by the gate.
//
// Keep this file dependent only on `std` + `semver` — `semver` is listed in
// both `[dependencies]` and `[build-dependencies]` so both units can see it.

use semver::Version;

/// Where the resolved `DAD_VERSION` came from. `build.rs` uses this to decide
/// whether the build deserves a `cargo:warning` (issue #250: a source build
/// that silently claims to be `0.1.0` is the bug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSource {
    /// A valid `DAD_VERSION` pre-set in the build environment by a packager
    /// or CI job.
    Injected,
    /// `git describe --tags --abbrev=0` in the build checkout — the path a
    /// release build and a normal dev checkout take.
    Git,
    /// `CARGO_PKG_VERSION`, i.e. the `0.1.0` placeholder in `Cargo.toml`.
    /// Last resort: nothing was injected and git yielded no usable tag.
    Placeholder,
}

/// A resolved `DAD_VERSION` plus the step of the resolution order that
/// produced it.
pub struct ResolvedVersion {
    pub value: String,
    pub source: VersionSource,
}

/// Is `value` safe to interpolate into a `cargo:` directive line?
///
/// Cargo parses build-script stdout **line by line**: an interior LF (or CR)
/// in an emitted value ends the current directive and promotes the remainder
/// of the value into a *second* directive of the attacker's choosing —
/// `rustc-env`, `rustc-cfg`, `rustc-link-arg`. A bare `cargo:` substring with
/// no line break is inert, so the line-protocol characters are what matter.
/// Other control characters cannot start a directive but can spoof or clear a
/// terminal reading the build log, so they are rejected too.
///
/// Every value this build script emits must pass this check first. Nothing
/// here is a privilege boundary for a packager who already controls the whole
/// build, but the point of issue #250 is CI/packaging automation, where a
/// narrowly-scoped untrusted value (a tag name, a PR field, a package
/// variable) may be forwarded into these variables — and that limited input
/// must not reach compiler or linker directives.
pub fn is_single_line_directive_value(value: &str) -> bool {
    !value.chars().any(char::is_control)
}

/// Render an untrusted value for inclusion in a `cargo:warning=` line.
///
/// [`str::escape_debug`] turns CR/LF into `\r`/`\n` and escapes control,
/// bidi-format and other non-printable codepoints, so a rejected value can be
/// *shown* to whoever is reading the build log without the value itself being
/// able to open a second directive line or reorder the message.
pub fn escape_for_cargo_warning(value: &str) -> String {
    value.escape_debug().to_string()
}

/// Normalize a candidate version string (a `DAD_VERSION` injection or a git
/// tag) into the value to bake in, or `None` when it is not a version we can
/// safely emit.
///
/// Strips a leading `v`/`V` (this project tags `v0.35.2`) and then validates
/// the remainder with **the same `semver::Version::parse` that
/// `src/version.rs` uses**. That equality is the point: `src/version.rs` does
/// `semver::Version::parse(env!("DAD_VERSION")).expect(…)`, so emitting a
/// value this parser rejects would panic the binary at startup — while
/// rejecting a value it *accepts* would send a correctly-injected packager
/// build down the `0.1.0` placeholder path and revive the very bug #250 fixes.
/// An invalid candidate falls through to the next resolution step instead.
pub fn normalize_version(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    let stripped = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);

    if Version::parse(stripped).is_err() {
        return None;
    }
    // SemVer's grammar is a strict subset of `[0-9A-Za-z.+-]`, so an accepted
    // value cannot carry a line break; assert the emission invariant anyway so
    // the two checks can never drift apart.
    if !is_single_line_directive_value(stripped) {
        return None;
    }

    Some(stripped.to_string())
}

/// Normalize a candidate `DAD_BUILD_ID` injection, or `None` when it is blank
/// or contains a character outside the accepted set.
///
/// The build id has no format to validate against — it is only ever compared
/// as an opaque string by the daemon-restart handshake — but it *is* emitted
/// as a `cargo:rustc-env` value and later rendered into terminals and logs, so
/// a conservative bounded alphabet applies: ASCII alphanumerics plus `.`,
/// `-`, `+` and `_`. The real shape is `<semver>-g<hex>[-dirty]`, and a
/// packager suffix such as `1.2.3-nixpkgs` or `1.2.3-1ubuntu1` fits
/// comfortably inside that set. Surrounding whitespace (from a shell export)
/// is trimmed; interior whitespace, CR/LF and control characters are
/// rejected rather than trimmed, which is what closes the directive-injection
/// hole (`foo\ncargo:rustc-env=INJECTED=yes` used to set both variables).
pub fn normalize_build_id(candidate: &str) -> Option<String> {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || !trimmed.chars().all(is_build_id_char) {
        return None;
    }
    Some(trimmed.to_string())
}

/// The bounded build-id alphabet: ASCII alphanumerics plus `.`, `-`, `+`, `_`.
fn is_build_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_')
}

/// Resolve the `DAD_VERSION` to bake into the binary, in the issue #250
/// order: a valid injected value, then the git tag, then the
/// `CARGO_PKG_VERSION` placeholder.
///
/// `injected` is the raw `DAD_VERSION` from the build environment and
/// `git_tag` the raw `git describe --tags --abbrev=0` output; both are
/// validated here, and an invalid one falls through to the next step rather
/// than being emitted.
pub fn resolve_version(
    injected: Option<&str>,
    git_tag: Option<&str>,
    cargo_pkg_version: &str,
) -> ResolvedVersion {
    if let Some(value) = injected.and_then(normalize_version) {
        return ResolvedVersion {
            value,
            source: VersionSource::Injected,
        };
    }
    if let Some(value) = git_tag.and_then(normalize_version) {
        return ResolvedVersion {
            value,
            source: VersionSource::Git,
        };
    }
    ResolvedVersion {
        value: placeholder_version(cargo_pkg_version),
        source: VersionSource::Placeholder,
    }
}

/// The SemVer build metadata a build that could not determine its own version
/// carries (issue #326). `src/version.rs` keeps the same value as
/// `UNKNOWN_VERSION_METADATA`, and `tests/build_version.rs` checks the two
/// agree.
pub const UNKNOWN_VERSION_METADATA: &str = "unknown";

/// The value a build with no injected version and no git tag reports: the
/// `CARGO_PKG_VERSION` placeholder marked `+unknown` (issue #326).
///
/// Without the mark that build reported a bare `0.1.0`, which every
/// version-aware consumer read as a genuine, very old release — and `v0.1.0`
/// is a real published tag, so the bare value cannot say "unknown" on its own.
/// Build metadata keeps the value valid SemVer, so `src/version.rs`'s
/// `.expect()` holds, and SemVer precedence ignores it, so a build that does
/// not look for the mark orders the value exactly as it ordered `0.1.0`.
pub fn placeholder_version(cargo_pkg_version: &str) -> String {
    let placeholder = cargo_pkg_version.trim();
    match Version::parse(placeholder) {
        Ok(mut version) => {
            version.build = semver::BuildMetadata::new(UNKNOWN_VERSION_METADATA)
                .expect("`unknown` is valid build metadata");
            version.to_string()
        }
        // Cargo validates `package.version` as SemVer, so this is not reached
        // by a real build; emit the value unchanged rather than invent one.
        Err(_) => placeholder.to_string(),
    }
}

/// Resolve the `DAD_BUILD_ID`: a valid injected value wins, otherwise compose
/// `<version>-g<short-sha>[-dirty]`, degrading to `<version>-unknown` when
/// git metadata is unavailable (PRD #103 M1.0's shape, unchanged).
///
/// An injection that fails [`normalize_build_id`] falls through to the
/// composed value exactly as if it had been absent — `build.rs` warns about it
/// separately.
pub fn resolve_build_id(
    injected: Option<&str>,
    version: &str,
    short_sha: Option<&str>,
    dirty: bool,
) -> String {
    if let Some(id) = injected.and_then(normalize_build_id) {
        return id;
    }
    match short_sha.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sha) => {
            let dirty_suffix = if dirty { "-dirty" } else { "" };
            format!("{version}-g{sha}{dirty_suffix}")
        }
        None => format!("{version}-unknown"),
    }
}
