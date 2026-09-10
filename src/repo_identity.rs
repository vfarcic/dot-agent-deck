//! Single source of truth for the upstream GitHub repository identity
//! (issue #945).
//!
//! The slug `vfarcic/dot-agent-deck` used to be spelled out at seven sites
//! across `src/version.rs`, `src/remote.rs` and `src/ui.rs`: the release feed,
//! the binary download base, two copies of a "star the repo" block, and the
//! star prompt's own popup line. A fork had to patch all seven and re-patch
//! whichever of them upstream happened to move — `src/ui.rs` in particular
//! churns constantly. All seven now derive from the one literal at the bottom
//! of this file, so re-pointing a build at another repository is a single edit
//! in a file upstream has no further reason to touch.
//!
//! **The two user-facing strings follow the same value, deliberately.** Issue
//! #945 asks for that to be decided rather than defaulted into: a fork's users
//! being invited to star *upstream* is the wrong outcome, so the
//! "Visit … to star ⭐" status message and the star popup's link line
//! re-point along with the release URLs.
//!
//! **Why a macro.** `concat!` accepts literals only — it cannot read a `const`
//! — so a plain `const SLUG` would leave each URL spelling the slug out again.
//! Expanding one macro with the slug as a `literal` fragment keeps all five
//! values `&'static str` compile-time constants, which is what
//! [`crate::remote::RELEASE_BASE`]'s call sites and `version.rs` already
//! expect, while the slug itself appears exactly once outside `#[cfg(test)]`
//! code. The other occurrences under `src/` — the rest of this file's test
//! module, and two in `src/ui.rs`'s — are byte-identity expectations that a
//! fork does not need to touch: they skip once the seam moves.
//!
//! Scope: this covers the repo identity compiled into the **binary**. The slug
//! is also written out in CI workflows, `Taskfile.yml`, `flake.nix`, the docs
//! site config, the README, PRDs and the changelog. None of those reach the
//! binary, several of them (workflow `repo:` targets, branch-protection and
//! release scripts) must keep naming the canonical repository, and the rest is
//! prose; they are deliberately left alone.

/// Expand the whole repo-identity surface from one `<owner>/<repo>` literal.
macro_rules! derive_repo_identity {
    ($slug:literal) => {
        /// `<owner>/<repo>` of the upstream repository.
        pub const SLUG: &str = $slug;

        /// Canonical repository URL — what the star prompt opens in the
        /// user's browser.
        pub const URL: &str = concat!("https://github.com/", $slug);

        /// Scheme-less form for TUI text, where a `https://` prefix is noise
        /// and costs columns in a 50-wide popup.
        pub const DISPLAY: &str = concat!("github.com/", $slug);

        /// GitHub API endpoint for the latest release, behind the upgrade
        /// nudge in [`crate::version`].
        pub const RELEASES_API_URL: &str =
            concat!("https://api.github.com/repos/", $slug, "/releases/latest");

        /// Base URL release assets hang off — `remote add` downloads the
        /// matching binary onto a remote host from under here.
        pub const RELEASE_DOWNLOAD_BASE: &str =
            concat!("https://github.com/", $slug, "/releases/download");
    };
}

// ─── THE SEAM ───
// Change this one line to point a build at a different repository.
derive_repo_identity!("vfarcic/dot-agent-deck");

#[cfg(test)]
mod tests {
    use super::*;

    /// The upstream slug, spelled out here so the assertion below compares
    /// against a literal rather than against a value derived the same way it
    /// is under test.
    const UPSTREAM_SLUG: &str = "vfarcic/dot-agent-deck";

    /// Issue #945 is a refactor with no intended behaviour change, so the bar
    /// is byte-identity with the literals it replaced — a changed release URL
    /// breaks self-update, and a changed display string changes what the TUI
    /// paints. These are those literals, written out rather than composed, so
    /// a broken derivation cannot silently agree with itself.
    ///
    /// Skipped rather than failed on a fork that has moved the seam: making
    /// this test a second place to patch would defeat the point of the module.
    #[test]
    fn upstream_values_are_byte_identical_to_the_literals_they_replaced() {
        if SLUG != UPSTREAM_SLUG {
            println!(
                "SKIP: the seam has been re-pointed to {SLUG}; upstream byte-identity does not apply"
            );
            return;
        }

        assert_eq!(SLUG, "vfarcic/dot-agent-deck");
        assert_eq!(URL, "https://github.com/vfarcic/dot-agent-deck");
        assert_eq!(DISPLAY, "github.com/vfarcic/dot-agent-deck");
        assert_eq!(
            RELEASES_API_URL,
            "https://api.github.com/repos/vfarcic/dot-agent-deck/releases/latest"
        );
        assert_eq!(
            RELEASE_DOWNLOAD_BASE,
            "https://github.com/vfarcic/dot-agent-deck/releases/download"
        );
    }

    /// The derivation itself, independent of which slug is baked in: a fork
    /// that edits the seam gets a consistently re-pointed set rather than a
    /// half-patched one. Written against `SLUG` so it keeps holding after the
    /// one-line edit this module exists to make cheap.
    #[test]
    fn every_derived_value_is_built_from_the_one_slug() {
        assert_eq!(URL, format!("https://github.com/{SLUG}"));
        assert_eq!(DISPLAY, format!("github.com/{SLUG}"));
        assert_eq!(
            RELEASES_API_URL,
            format!("https://api.github.com/repos/{SLUG}/releases/latest")
        );
        assert_eq!(
            RELEASE_DOWNLOAD_BASE,
            format!("https://github.com/{SLUG}/releases/download")
        );
    }
}
