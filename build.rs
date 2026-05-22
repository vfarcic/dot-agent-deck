use std::process::Command;

fn main() {
    // Derive version from git tags (e.g. "v0.7.1" -> "0.7.1", "v0.25.0-alpha.0" -> "0.25.0-alpha.0").
    // Falls back to CARGO_PKG_VERSION when not in a git repo or no tags exist.
    let version = git_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=DAD_VERSION={version}");

    // Re-run if HEAD changes (new commit, new tag, checkout, etc.)
    println!("cargo:rerun-if-changed=.git/HEAD");
}

fn git_version() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let tag = String::from_utf8(output.stdout).ok()?.trim().to_string();
    let stripped = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(&tag);

    // SemVer check: digits.digits.digits, optionally followed by a `-<prerelease>`
    // suffix (alphanumeric + dots/dashes per the SemVer grammar). The pre-release
    // suffix is accepted opaquely — we only validate the X.Y.Z core.
    let core = stripped.split_once('-').map(|(c, _)| c).unwrap_or(stripped);
    let core_parts: Vec<&str> = core.split('.').collect();
    if core_parts.len() == 3 && core_parts.iter().all(|p| p.parse::<u64>().is_ok()) {
        Some(stripped.to_string())
    } else {
        None
    }
}
