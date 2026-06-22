# dot-agent-deck desktop GUI (PRD #176)

An opt-in **second front-end** to the dot-agent-deck daemon (the TUI is the first). It is a [Tauri](https://tauri.app) app: a thin Rust shell hosting a webview, layered over a plain-Rust core that speaks the same daemon wire protocol the TUI uses. The GUI holds no business logic — it is a fourth client of the daemon (Design Decision #1).

This is **preview / opt-in** software, built deliberately and not part of the default `dot-agent-deck` release artifacts.

## Layout

| Path | What it is | In the Rust gates? |
|---|---|---|
| `core/` (`dad-gui-core`) | Plain-Rust lib: socket discovery, connect, `Hello` version negotiation, frame bridge. No webview deps. | **Yes** — a default workspace member; `cargo fmt`/`clippy`/`test-fast` compile and test it. |
| `src-tauri/` (`dad-gui`) | Thin Tauri shell: window + webview + IPC, layered over `dad-gui-core`. | **No** — workspace-`exclude`d (needs WebKitGTK; see below). |
| `dist/` | Hand-authored vanilla web frontend (`index.html` / `main.js` / `styles.css`) + vendored xterm.js under `dist/vendor/xterm/`. No bundler. | n/a |
| `package.json` | JS toolchain entry point (`@tauri-apps/cli`, `@xterm/*`). Contained here; never leaks to the repo root. | n/a |
| `nix/` | Local flake (`tauri-build-deps`) provisioning the WebKitGTK dev/build toolchain for Linux. Referenced from the repo-root `devbox.json`. | n/a |

The split is deliberate: the testable logic lives in `core/` so the Rust gates stay authoritative over it, while the shell that needs the system webview libraries is isolated so a missing dev library can never break `cargo fmt`/`clippy`/`test-fast` for the rest of the repo (PRD #176 Risks).

## What M1.2 + M1.3 do

`dad-gui-core` discovers the daemon's attach socket (via the shared `protocol::attach_socket_path`, the exact path the TUI uses), connects, performs the `Hello` version negotiation, lists agents, opens **attach streams**, and coalesces resizes — all over the same wire types the TUI uses (Design Decision #1).

The frontend shows **connected + negotiated protocol/daemon version** (or a connect/retry state), an **agent picker**, and — M1.3 — a **live embedded terminal** (xterm.js): selecting an agent opens its PTY stream, daemon `KIND_STREAM_OUT` bytes paint the terminal (scrollback + truecolor), keystrokes flow back as `KIND_STREAM_IN`, and terminal resizes are forwarded to the daemon's PTY with single-slot latest-wins coalescing (mirroring `embedded_pane.rs`). PTY bytes cross the Tauri IPC boundary base64-framed both directions so arbitrary control bytes stay exact.

## Toolchain (provisioned via devbox / nix — not apt)

The Tauri build needs WebKitGTK 4.1 + companions on **Linux**; **macOS** uses the system WKWebView and needs none of them. Both are provisioned through nix so the same `devbox.json` resolves on either OS (the maintainer develops on both). Because `devbox install` fetches only each package's *runtime* output — never the `-dev` output that holds the `.pc` files pkg-config needs — the webview libraries are provisioned through a small local flake, `gui/nix` (`tauri-build-deps`), that re-exports the **dev** outputs as one transitively-closed pkg-config environment. The repo-root `devbox.json` references it as `path:gui/nix#tauri-build-deps`, scoped with `excluded_platforms` to the two darwin targets, alongside generic `pkg-config` + `patchelf`.

Net effect: a single `devbox install` makes `pkg-config --exists webkit2gtk-4.1` succeed inside the devbox env, with **no** `apt` (or Homebrew) steps.

## Building & running

From the repo root, with [devbox](https://www.jetify.com/devbox) installed:

```bash
devbox install                 # provision the toolchain (pulls WebKitGTK + companions from the nix cache)
devbox shell                   # enter the env: pkg-config finds webkit2gtk-4.1, Rust toolchain on PATH
cd gui && npm install          # fetch the Tauri CLI + xterm.js (contained to gui/)
npm run dev                    # tauri dev — launch the GUI against your running daemon
# or, the bundler-less path:
( cd src-tauri && cargo run )
npm run build                  # tauri build — produce the desktop binary
```

The daemon is located by the same always-external-daemon rules the TUI uses (PRD #93); if none is reachable the GUI shows a connect/retry state. To see a **live** terminal, run a daemon with at least one agent — e.g. start the TUI (`dot-agent-deck`) and spawn an agent, then launch the GUI and click that agent in the picker.

## Why the Rust gates don't build the shell

`cargo` at the repo root never compiles `src-tauri/` (it is in the workspace `exclude` list, and the crate carries its own empty `[workspace]` table so a standalone `cargo build` inside it doesn't re-discover the root), so the absence of WebKitGTK on a machine only blocks the GUI build here — not the Rust CI gates. The full toolchain doc lands in `docs/develop/` at M5.3.
