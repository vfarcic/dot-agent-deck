# Self-upgrade: design and trust boundary

How `dot-agent-deck upgrade` and the desktop app's upgrade dialog notice a newer release and upgrade the copies installed on this machine (issue #1635), what each step verifies, and what the verification does not cover. The user-facing behaviour is in the published docs; this page is for whoever changes `src/self_upgrade/` or `desktop/src-tauri/src/self_upgrade.rs`.

## The seams

Everything lives in one root-crate module, `src/self_upgrade/`, which both clients call, so the TUI, the CLI and the desktop app use the same words (CLAUDE.md rule 22). The desktop crate reaches it through its path dependency on the root package.

| file | job |
| --- | --- |
| `detect.rs` | how one copy was installed. `detect` is a pure function of `DetectInputs`; `inspect` gathers the real inputs through a `Host`. |
| `plan.rs` | what to offer for that install and a newer release, and every line a client shows, as `PlanLine::Text` / `PlanLine::Command`. Pure. |
| `discover.rs` | the other copy on the machine: the desktop app from the CLI, the CLI from the desktop app, each inspected through its own install method. |
| `verify.rs` | the checksum manifest, build provenance through `gh attestation verify`, the new binary's `--version`, and the re-hash before a staged file is installed. |
| `execute.rs` | the private staging directory, downloading, and carrying out a confirmed plan. |
| `cli.rs` | `dot-agent-deck upgrade [--check\|--yes]`. |

Every subprocess and filesystem question the detection and planning steps ask goes through the `Host` trait, so a unit test fakes the machine; `SystemHost` is the real one. The staged downloads are real files, so the execute tests use temporary directories.

## Detection

`detect::detect` decides the install method from the canonical executable path and a few facts about it, in this order, first match wins:

1. under `/nix/store` → Nix (notify only);
2. inside `<prefix>/Cellar/<formula>/` for one of the tap's formulas, where `<prefix>` is a default Homebrew prefix or the one `brew --prefix` reports → Homebrew (`brew upgrade <formula>`);
3. owned by a Debian package: the desktop `.deb`'s package → the desktop `.deb` (its bundled CLI upgrades with it); any other package → a system package (notify only);
4. a source build (a cargo build directory, a `cargo install` location, a dirty checkout's build id) → notify only;
5. on macOS, inside a `.app` bundle → the desktop `.dmg` install (swapped in place when its folder is writable and the running app is signed; otherwise the user downloads the new image);
6. otherwise a downloaded binary, replaced in place when its directory is writable, and staged for a privileged install when it is not.

## The release channel

`release_channel` (`src/self_upgrade/mod.rs`) puts a copy on one of two channels, `ReleaseChannel` in `src/version.rs`. A copy installed from the `dot-agent-deck-beta` Homebrew formula, or whose own version is a SemVer prerelease (`0.47.0-beta.1`), is on the prerelease channel; every other copy is on the stable channel. The stable channel asks GitHub's `releases/latest`, which never names a prerelease, and refuses an answer that is a draft or a prerelease anyway. The prerelease channel asks the `releases` list endpoint and takes the highest SemVer among the releases that are not drafts, so a beta is offered the next beta, and a stable release once it is higher than every beta. A client looks the release up once, on its running copy's channel, and plans the other copy on the machine against the same release, so the CLI and the desktop app on one machine follow one channel. The TUI's startup notice uses the same lookup, choosing the channel from its own version. Under the `e2e` feature, `DOT_AGENT_DECK_TEST_RELEASES_API_URL` and `DOT_AGENT_DECK_TEST_RELEASES_LIST_API_URL` replace the two endpoints for an L2 test.

The beta formula only ever receives prereleases (`release.yml`'s "Detect channel" step), so once a stable release is the highest version, `brew upgrade dot-agent-deck-beta` cannot reach it: the plan still offers it, and the formula stays where it is. Moving to the stable formula is the user's step (`docs/installation.md` says the two conflict).

## The verification chain

For every plan that downloads something, in order:

1. **Provenance availability, before confirmation.** `ProvenanceCheck::detect` looks for `gh` and runs `gh auth status`, and the client puts the answer in `PlanOptions::provenance`. The plan carries it and says it in words: either the manifest's provenance will be checked with `gh attestation verify`, or it will NOT be, and why. A known logged-out state (`not logged in`, an invalid token) and an unexpected failure (`gh` cannot be spawned; `gh auth status` failing for another reason, such as no network) each get their own reason.
2. **Staging.** `Staging::create` makes a fresh directory `v<version>-<random>` with mode `0700`, created exclusively, inside the staging root (`upgrade/` under the deck's state directory). The root itself is refused unless it is a real directory (not a symlink), owned by the current user and not writable by group or others. Every staged file is created new with `O_EXCL` and `O_NOFOLLOW`, so nothing already at a path, a planted symlink included, is written through.
3. **The manifest.** `checksums.txt` (or `checksums-desktop-alpha.txt`) is downloaded into memory and written once into the staging directory for `gh` to read.
4. **Provenance.** When the plan promised it, `gh auth status` is asked again; if `gh` has since disappeared or logged out, the upgrade aborts rather than silently downgrading. Then `gh attestation verify <manifest> --repo <slug> --signer-workflow <slug>/.github/workflows/release.yml --source-ref refs/tags/v<version> --format json`. The JSON must name the SHA-256 of the manifest bytes held in memory as an attestation subject: `gh` reads the file while the checksums are parsed from memory, and that digest is what ties the two together.
5. **Checksum.** The asset is downloaded into memory and its SHA-256 must match the manifest's single entry for it. A missing, ambiguous or mismatched entry aborts.
6. **`--version`.** A CLI binary must answer as dot-agent-deck at the release's version (for the `.dmg`, the CLI bundled in the new app; the app also passes `codesign`, the running app's Team ID and Gatekeeper).
7. **Re-hash at the hand-off.** Right before the staged file is given to whatever installs it (the rename over a writable binary, `pkexec install`, `pkexec apt-get install`, `hdiutil attach`), it is hashed again against the verified digest, and a mismatch aborts.
8. **After a privileged binary install**, the installed target, now root-owned, is hashed against the verified digest before it is run for its `--version`. A mismatch is reported as an installed file that is not the verified build and must be reinstalled, and the file is not executed.

A command the user runs themselves (`sudo install …`, `sudo apt install …`) is only built once the download is verified, because the staging directory's name is not known before that. It starts with a checksum check of the staged file, `echo '<sha256>  <path>' | sha256sum -c - && sudo …` (`shasum -a 256 -c -` on macOS), so `sudo` runs only on the verified bytes. No command is built for a path that is not UTF-8, is longer than `MAX_SHOWN_PATH_CHARS`, or contains a control character, a bidi or other invisible formatting character, or a backslash (which `sha256sum -c` reads as an escape); the client shows how to upgrade manually instead. The desktop app keeps a command's exact text apart from its display copy, and offers Copy only when its own display sanitizer would leave the command unchanged.

### Why `--source-ref` is safe to require

Checked on 2026-10-10 against the published attestations: `checksums.txt` and `checksums-desktop-alpha.txt` of v0.46.0, v0.45.1, v0.45.0, v0.44.0 and v0.43.0 all verify with `--source-ref refs/tags/v<version>`, and v0.46.0's manifest checked against `refs/tags/v0.45.1` fails with `expected SourceRepositoryRef to be refs/tags/v0.45.1, got refs/tags/v0.46.0`. Releases are cut by pushing a `v*` tag (`tag-release.yml`), so the attestation's source ref is the tag. `release.yml` also has a `workflow_dispatch` trigger; a release cut that way would be attested with the dispatching branch as its source ref, and an upgrade to it fails provenance and changes nothing, which is the intended failure direction. Among the last 100 `release.yml` runs (listed with `gh run list --workflow release.yml`), one dispatched run succeeded, on 2026-04-27, months before self-upgrade existed. An upgrade only ever targets the newest release on the running copy's channel (next section).

## The residual same-user race

Every check above defends against a file changing between the network and the install, and against another user on the machine. None of them defends against a process already running as the same user, and that is a deliberate boundary rather than an oversight.

Such a process can replace a staged file in the window between the re-hash and the moment `pkexec install`, `apt-get` or `hdiutil` opens it. The narrow mitigations were chosen over a root-side installer that snapshots and verifies the file itself, because a same-user process can already intercept any privileged operation the user starts: it can edit the shell's rc files or put its own `sudo` or `pkexec` earlier on `PATH`, and wait for the user to type their password. A root-side snapshot helper would move the check across the privilege boundary without moving the trust boundary, which is the user's own account. What the mitigations buy is that the window is short, that a swap made before it is caught, and that a mismatch which reaches a root-owned target is reported and never executed.

The same reasoning covers the `--version` run of a staged CLI: it executes a file in a directory only the user can write.

Build provenance is only as strong as `gh` is: a plan run where `gh` is missing or logged out still checks the checksum against the manifest, and says before confirmation that provenance will not be checked.
