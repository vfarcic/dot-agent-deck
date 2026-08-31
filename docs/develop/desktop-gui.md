# Desktop GUI developer preview

The desktop GUI under `desktop/` is an opt-in Tauri preview for PRD #176. It is a second local client of the existing daemon, not a replacement for the TUI, and it is not included in the default release artifacts. The current M0/M1 spike deliberately depends on the root `dot-agent-deck` library before the protocol crate is extracted so that terminal transport and the control-deck interaction model can be tested first.

## Prerequisites

- Enter `devbox shell` for the repository's pinned toolchain: Rust 1.97.1, `cargo-nextest`, Clippy, rustfmt, Node.js 24.12.0, and pnpm 10.34.5. Provide equivalent versions yourself if you do not use Devbox — the frontend needs Node.js 20.19 or newer, and pnpm 10.x, which is the line that reads `desktop/pnpm-lock.yaml`'s `lockfileVersion: '9.0'` without rewriting it. CI's `desktop-web` job deliberately runs Node 20 rather than the Devbox pin, so the stated floor stays tested rather than merely claimed.
- Install the [Tauri 2 system prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform. On macOS this includes the Xcode command-line tools; Linux needs the documented WebKitGTK and related development packages.
- Install the desktop JavaScript dependencies once:

```sh
cd desktop
pnpm install
```

Agent CLIs and their credentials are needed only for agents you deliberately start through the daemon. The fixture preview does not call an LLM, execute an agent command, or modify project files.

## Fixture preview

Run the web frontend without Tauri for a deterministic, safe UI fixture:

```sh
cd desktop
pnpm dev
```

Open `http://localhost:1420/`. A normal browser defaults to fixture transport; `http://localhost:1420/?fixture=1` selects it explicitly. Use `state=disconnected`, `state=error`, or `state=empty` to inspect failure and empty states, for example `http://localhost:1420/?fixture=1&state=disconnected`.

The fixture's **Advance fixture** control walks a fixed review → test → human-approval sequence. Terminal input, pause/resume, retry, approval, workflow ordering, and agent-profile editing affect fixture or browser-local state only.

## Live Tauri preview

The live preview requires a daemon built from the same checkout. The exact build match is intentional: the desktop refuses to attach across a protocol or build mismatch and never recycles a daemon that may own valuable running agents.

Build the matching CLI once so the desktop can resolve a sibling executable:

```sh
devbox shell
cargo build --locked --bin dot-agent-deck
```

Then start the desktop window:

```sh
devbox shell
cd desktop
pnpm tauri dev
```

Tauri selects live transport automatically and initially performs a connect-only probe. If no daemon owns the socket, use the visible **Start daemon** action; daemon creation never happens merely because the window opened. The Rust core resolves only a pre-launch `DOT_AGENT_DECK_BINARY`, a sibling `dot-agent-deck` binary, or one already on `PATH`—the webview cannot supply an executable path. You may instead run `cargo run --locked --bin dot-agent-deck -- daemon serve` in another terminal and leave it running.

If another build already owns the default socket, do not stop it until you have confirmed that terminating its agents is safe; use `dot-agent-deck daemon stop` only as a deliberate lifecycle action.

### Attaching across a build stamp difference (development only)

The handshake makes two checks, and they are not equally strict. `PROTOCOL_VERSION` must match — that is the wire contract. The **build stamp** (`git describe`) must then match too, which is right for a shipped app but hostile in development: every commit restamps the desktop on its next rebuild, and a released daemon never matches a branch build at all. The result is that the daemon you actually use is the one daemon the preview refuses to open.

Set `DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH=1` before launching to downgrade the **stamp** difference from a refusal to a warning:

```sh
DOT_AGENT_DECK_DESKTOP_ALLOW_BUILD_MISMATCH=1 pnpm tauri dev
```

The protocol check is unaffected and still refuses an incompatible daemon, and the mismatch is not swallowed — the connection banner keeps naming both builds for the whole session. Only `1` and `true` arm it.

This is a **development** switch, not a compatibility guarantee. Two builds can agree on `PROTOCOL_VERSION` and still disagree about what a field means — precisely the semantic break `CLAUDE.md` rule 12 describes — so the stamp check is what protects a daemon that owns live agents from a client that reads its state differently. Use it to inspect a daemon, not to run work you care about, and never in a packaged build.

In live mode the preview lists daemon-owned agents, attaches xterm.js to each PTY stream, forwards terminal input and resize requests, refreshes status, and exposes a confirmed stop action. Open **Workflows**, provide the exact orchestration name from the target project's `.dot-agent-deck.toml` and an absolute project directory, then choose **Launch live loop** to start its configured role set with the enabled profile commands. **Start daemon** and **Launch live loop** both require a confirmation. The project configuration must contain exactly the submitted roles and one start role; the current bundled `dot-agent-deck` profile uses `orchestrator` as that start role and requires every listed profile to remain enabled for a live launch. Before spawning, the desktop materializes the same canonical coordinator context used by the TUI. Non-Pi coordinators use a readiness-gated, identity-bound, idempotent submission with bounded retry. Pi cannot be the desktop workflow coordinator in this preview: its native seed path has no delivery acknowledgement, so the bridge rejects that launch before spawning any role. Use a non-Pi coordinator or launch that orchestration from the TUI until acknowledged native seed delivery is available. A partially-created workflow—or one whose coordinator context cannot be delivered—is rolled back in reverse role order.

Desktop bundles include `dot-agent-deck` as a Tauri sidecar built from the same checkout. Run `pnpm bundle:app` to prepare the matching sidecar and produce the native app; the separate bundle config keeps ordinary workspace `cargo test` runs independent of generated binaries. A build/protocol mismatch exposes **Replace daemon** only when the old daemon reports zero live agents; replacement uses the bundled binary and never force-stops live agents.

The **Projects** rail opens a device-local project library. Each entry stores a display name, absolute repository directory, orchestration name, and notes. Selecting an active project updates the control-deck context and prefills the workflow launch form. Removing an entry uses a two-step confirmation and only removes local desktop metadata; it never deletes or moves the repository. Projects still need a matching `.dot-agent-deck.toml`, and launch remains the authoritative validation boundary.

Live workflow launch from the desktop preview is currently supported on macOS and Linux only. On Windows, the workflow sheet detects the platform, explains the limitation, and disables **Launch live loop**; the Rust bridge independently rejects a crafted `StartWorkflow` IPC request before validating or spawning roles. This guard is intentional because generated profile commands use POSIX shell quoting and must not be passed to `cmd.exe`. The fixture, daemon connection, and existing-agent terminal views remain available on Windows; use the TUI or launch commands manually until native Windows command construction is implemented.

Whole-run pause, fixture advancement, approval, and retry are not sent to the daemon. Workflow ordering remains a local preview and the live launch follows the role order in `.dot-agent-deck.toml`; command overrides apply to that launch only and do not rewrite project configuration.

## Release bundles

Every tagged release publishes the GUI as an **unsigned alpha** bundle, built by `release.yml`'s `desktop-bundle` job: a `.dmg` for macOS arm64 and a `.deb` for Linux x86_64. They arrive on the release as `dot-agent-deck-desktop-alpha-<platform>.<ext>` with their own `checksums-desktop-alpha.txt`, and the release notes carry a fixed section saying they are unsigned and how to clear the OS warning. There is no user-facing install page on purpose while the GUI is alpha; [#765](https://github.com/vfarcic/dot-agent-deck/issues/765) writes one when that changes.

Three things about that job are worth knowing before editing it, because each is a single line that reads as housekeeping and is not. It hangs off `prepare` rather than `build`, so it runs beside the CLI matrix instead of extending the critical path. `finalize` — which creates the release and uploads the five CLI assets — deliberately does **not** list it in `needs:`, so a bundler that fails, hangs, or is skipped cannot delay or break an ordinary tag; the run goes red while the release stays green and complete. And `finalize`'s artifact download is constrained by `pattern: dot-agent-deck-*` because `merge-multiple: true` would otherwise flatten the desktop bundles into the same `dist/` directory, where the release glob would sweep them up and `task checksums`' `shasum -a 256 dot-agent-deck-*` would choke on the space in Tauri's `Agent Deck_<version>_amd64.deb`. All three are asserted by `xtask/linkage-check/src/release_workflow_wiring.rs`, since nothing else can check a workflow that only fires on a tag.

`tauri.conf.json` hardcodes `version: "0.1.0"` — the same deliberate placeholder as `Cargo.toml`. The job merges the real version into the bundle config at build time with `jq` and then fails the build if no output filename carries it, so a bundle labelled `0.1.0` cannot be published. `bundle.active` is likewise `false` in the base config and only the overlay flips it, which means an invocation that loses its `--config` produces no bundle *and still exits 0*; the job asserts the output exists rather than trusting the exit code.

**Windows is not built.** A bundle carries the daemon as a sidecar and no Windows daemon binary is published; a Windows GUI cannot attach to a remote Linux daemon instead, because the IPC transport is a named pipe on Windows and a Unix socket everywhere else. `prepare-sidecar.sh` nonetheless handles the `.exe` suffix on `*-pc-windows-*` triples — in both the cargo output path and the staged name Tauri resolves as `{path}-{triple}{ext}` — so that groundwork is done when [#741](https://github.com/vfarcic/dot-agent-deck/issues/741) makes a Windows GUI reachable. It is covered by `xtask/linkage-check/src/sidecar_staging.rs`.

Bundling locally needs more than the compile set `ci.yml` installs: on Linux the `deb` bundler additionally wants `patchelf`, `fakeroot`, `file` and `desktop-file-utils`.

AppImage is deliberately not built. Its bundler downloads five third-party artifacts at build time, two of them shell scripts from a `master` ref, which is not a supply-chain property a release pipeline should have; it also failed on the first real attempt while the `.deb` from the same run verified clean.

One trap if you bundle locally in a `devbox shell`: the shell runs under nix's glibc, whose `ld.so.cache` does not exist, so `ldconfig -p` returns nothing and system libraries are invisible to the loader. Exporting `LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu` fixes the Rust side but then breaks nix's `node` (`undefined symbol: uv_tcp_keepalive_ex`, from picking up the system libuv). Bundle without it; the Rust link resolves through pkg-config's `-L` flags and does not need it.

## Security and ownership model

The daemon remains the single source of truth for agent lifecycle, PTYs, hooks, and orchestration. The desktop opens no HTTP API or TCP listener: the Rust core connects to the same per-user local IPC endpoint as the TUI, verifies the endpoint's ownership/permissions, performs the existing `Hello` protocol and build handshake, and then bridges typed snapshots and bounded terminal chunks through Tauri IPC. PTY output travels daemon → Tauri channel → xterm.js; focused terminal input and resize requests travel in the opposite direction.

The window uses a restrictive content-security policy and a minimal Tauri capability set. Bridge commands are scoped to the main webview, connection errors are sanitized before reaching it, terminal input and launch commands are bounded to 64 KiB, dimensions are constrained to `1..=4096`, and terminal sessions use opaque IDs and are detached when the bridge is disposed. A build mismatch is shown as incompatible rather than triggering an automatic daemon restart.

Treat every live terminal as equivalent to its underlying agent CLI: it has the daemon user's filesystem and process permissions, and terminal input can authorize consequential work. The stop control has a confirmation step, but this preview is not a sandbox or an access-control boundary.

## Models and agent profiles

The fixture seeds an orchestrator and release profile using Claude and coder, reviewer, auditor, and tester profiles using `gpt-5.6-sol` with role-appropriate reasoning effort. Editing a profile stores a draft in the webview's local storage and **Confirm draft** does not execute anything or write `.dot-agent-deck.toml`; **Launch live loop** does execute each enabled profile's launch command through the daemon after the workflow validation described above. Treat launch commands as executable configuration, never store tokens in them, and review them before starting a live loop. **Reset defaults** clears the local draft.

For OpenAI, Anthropic, and OpenCode profiles, the launch command is generated from the current provider, CLI, model, reasoning-effort, and permission fields. The generated command is a read-only preview; on macOS and Linux, values are POSIX-quoted as individual shell words and invalid, NUL-containing, blank, or oversized commands block launch. An **advanced custom command override** is available for unusual CLIs, but it is explicitly labeled as an exact shell command that bypasses the structured fields. Its permissions are unmanaged by the profile UI, may be arbitrary, and must be encoded and reviewed in the command itself. Custom roles are excluded from structured full-access counts, and a custom override does not bypass the Windows platform guard. The launch confirmation distinguishes custom-command risk from permission claims made only for generated roles. The submitted command overrides the matching project role for that launch only; format-preserving profile write-back is not implemented.

Live sessions continue to use the commands and models that created them through the daemon. The current daemon snapshot exposes agent type but not a reliable model identifier, so the live UI labels model, cost, token, branch, and lease fields as unavailable instead of guessing.

## Current milestone limits

- The Tauri crate directly reuses the root library; the standalone protocol-crate extraction remains pending.
- **Local daemons only.** The GUI assumes the daemon is on the same machine. A forwarded socket does attach and streams remote agents and their PTYs correctly, but project resolution stays client-side, so a remote workflow launch reads the wrong `.dot-agent-deck.toml` and writes coordinator context to the wrong host. Treat remote as observation-only until that moves daemon-side.
- The deterministic loop, transition evidence, diffs, checks, artifacts, spend, and profile/workflow editors are preview data or local-only UI where the daemon does not expose structured data yet.
- Live orchestration graph events (`delegate`, `work-done`, and `dispatch`) and the graph view remain pending.
- OS-native notifications, signing, notarization, auto-update, remote/web hosting, and mobile layouts are out of the current spike. Packaging is no longer — see **Release bundles** above — but the bundles it produces are unsigned, so every OS still warns on first launch.
- Native Windows workflow-command construction is pending; the webview and Rust bridge both block desktop workflow launch on Windows rather than forwarding POSIX-quoted commands to `cmd.exe`.
- Pi coordinator launch is blocked in the desktop preview pending an acknowledged native seed-delivery protocol; Pi may still be used for non-start worker roles.
- Daemon startup is an explicit user action; visual rename and standalone start-agent flows are not exposed by the current frontend.
- Multi-pane throughput still needs the explicit M1.3 stress qualification, and a real-agent terminal session still needs the PRD's pre-release end-to-end gate.

## Verification commands

Run frontend tests and the production web build from `desktop/`:

```sh
pnpm test
pnpm build
```

Run the desktop Rust bridge tests and lints from the repository root:

```sh
cargo test --locked -p dot-agent-deck-desktop
cargo fmt --check
cargo clippy -p dot-agent-deck-desktop --all-targets -- -D warnings
```

Run the repository-wide required fast gates from the repository root:

```sh
cargo clippy -- -D warnings
cargo test-fast
```

Before a PR, run the local-only PTY/real-agent tier once, after the fast gates are green:

```sh
cargo test-e2e
```

The live manual smoke check is: build the matching CLI, launch `pnpm tauri dev`, use **Start daemon** if needed, launch the configured live loop against a disposable project/worktree, confirm every role hydrates under one orchestration, interact with a real agent in an embedded terminal, resize the window and terminal, reconnect without duplicated output, and stop only a disposable agent through the confirmation dialog. Do not treat the fixture as proof that a real agent or daemon path works.
