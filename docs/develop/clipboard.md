# Copying a pane selection to the clipboard

How the TUI delivers text a user selects in a pane (PRD #385), and the developer knob that keeps tests off the real clipboard. The user-facing description belongs on the published pages; this page is the mechanism.

## Two routes, every copy

`src/clipboard.rs` sends each copy two ways and records which went out:

- **Native**: the system clipboard through `arboard` (X11 through `x11rb`, Wayland through `wl-clipboard-rs` when `WAYLAND_DISPLAY` is set, falling back to X11; the pasteboard on macOS). It lands whatever the terminal is, but only on the machine the deck runs on, and only where that machine has a clipboard to open.
- **OSC 52**: the escape sequence written to the controlling terminal (`/dev/tty`), which crosses ssh but lands only where the terminal honours it.

The native write is attempted first; OSC 52 is sent every time. The status line is derived from the result by `clipboard::status_message`: "Copied to clipboard" only when the native write confirmed it, the OSC 52 wording (which points at Troubleshooting) when only the sequence is known to have gone out, a "not copied" line when neither went out, and nothing for an empty selection.

An agent's own OSC 52 copy (issue #1705) is not part of this: it is still passed through as OSC 52 only, from the per-frame drain in `ui.rs`.

## Threading and lifetime

The native clipboard is opened lazily on a dedicated `native-clipboard` thread at the first copy, and that thread keeps it until the deck exits, because on X11 and Wayland the process that made a copy is the one that serves it to a paste. Opening and writing can block (an X server that is slow or unreachable), so the UI thread waits at most `NATIVE_WRITE_WAIT` (250 ms) for an answer, then reports the write as not confirmed. While an earlier write is still running, a copy does not wait at all.

**The newest copy wins.** Copies made while a write is stuck do not queue behind it: one slot holds the newest copy not yet started, and a newer copy replaces it. Each copy carries a generation number, and the worker writes a copy only while it is still the newest, so once the clipboard answers again it writes the latest selection and drops the superseded ones (`clipboard/native/001`). The write that was already in progress cannot be stopped: the wait bound only stops the UI from waiting on it, and when the backend finally returns, that older text does reach the clipboard, just before the newest copy replaces it. If the user copied something in another application in the meantime, that late write can still overwrite it.

**Exit.** On X11, arboard asks a clipboard manager to take over the deck's copy when the clipboard object is dropped (the `SAVE_TARGETS` handoff, which itself waits up to 100 ms for the manager), so a copy can outlive the deck where a clipboard manager runs. On an ordinary exit, `run_tui` shuts the clipboard down after restoring the terminal and waits at most `NATIVE_SHUTDOWN_WAIT` (300 ms) for the worker to finish, which includes that drop; `NativeClipboard`'s `Drop` does the same bounded wait on the other ways out of `run_tui` (`clipboard/native/002`). Keeping a copy after the deck exits is best effort: past the deadline the deck exits without the handoff, an exit that skips both (the terminal-hangup path's `process::exit`, a signal) gets none, and without a clipboard manager there is nothing to hand over to.

**A panic on the clipboard thread is contained.** The worker runs under `catch_unwind` for its whole life and marks itself with a thread-local (`clipboard::in_clipboard_worker`) that the `run_tui` panic hook checks alongside the guarded vt100 feed's, so a panic while opening or writing (arboard's X11 constructor starts a thread with `std::thread::spawn`, which panics if the OS refuses one) does not tear the terminal down under a running TUI. The native clipboard is then settled as unavailable and copies go out as OSC 52 only (`clipboard/native/003`). A panic on a thread arboard starts for itself does not carry that mark, so it is not covered.

A failed open (no display, a headless session) is settled for the rest of the deck's life: later copies go out as OSC 52 only without trying to open the clipboard again.

## The test knob: `DOT_AGENT_DECK_NATIVE_CLIPBOARD`

Setting `DOT_AGENT_DECK_NATIVE_CLIPBOARD` to `0`, `false`, `off` or `no` turns the native write off, so copies go out as OSC 52 only. It is read once, when the TUI starts.

The e2e harness sets it to `0` for every deck it spawns (the pinned environment in `tests/common/mod.rs`). On Linux the harness's `env_clear` already removes `DISPLAY` and `WAYLAND_DISPLAY`, so the native open fails there anyway, but the macOS pasteboard needs no display variable, and without the knob a PTY test that copies a selection would overwrite the developer's real clipboard. In-process, the deck's clipboard (`SystemClipboard`) is constructed only in `run_tui`, which only the binary calls, and `clipboard::tests` drive the `ClipboardBackend` and `NativeSink` seams with fakes.

## Dependency

`arboard` is built with `default-features = false` (no image support) and `wayland-data-control`. Both backends on Linux are pure Rust; `wayland-sys` is pulled in with no features, so its build script probes nothing and links nothing. `Cargo.toml` carries the reasoning next to the dependency.
