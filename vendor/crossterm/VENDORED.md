# crossterm 0.29.0, patched

This directory is crossterm 0.29.0 as published on crates.io (upstream commit `36d95b26a26e64b0f8c12edfe11f410a6d56a812`), with one code change, in `src/event/source/unix/mio.rs` ("Checking it" lists the rest). The root `Cargo.toml` puts it in place of the registry crate with `[patch.crates-io]`, so both the deck and `ratatui-crossterm` build against it. It is excluded from the workspace, so the workspace's fmt and clippy gates do not run on it.

## What the patch fixes

A key typed right after a terminal resize could sit unread until the user typed something else. The TUI looked frozen to that key; the next keystroke released it.

The mio event source handles one batch of readiness events per `poll`. Upstream returned from inside that loop as soon as it met the SIGWINCH token, or as soon as the terminal token yielded one parsed event. Any token later in the same batch was never handled, and mio's readiness is edge-triggered (epoll `EPOLLET` on Linux, kqueue `EV_CLEAR` on macOS), so it was not reported again: unread terminal bytes stayed unread until new input raised a new edge, and a SIGWINCH queued behind input waited for the next resize. A batch holds both when a resize and a keystroke arrive while the event loop is busy, for example while it is drawing the frame for the resize before.

The patched loop handles every token in the batch first: for the terminal it reads as upstream did and stops reading where upstream returned, once the parser holds an event; for a SIGWINCH it queues a `Resize` in the parser's event queue in batch order; then it returns the first queued event. The next call returns the rest before it polls again. An error from measuring the terminal or a waker interrupt is returned after the batch rather than in the middle of it, so input read in the same batch stays queued for the next call.

The terminal descriptor is blocking, so the read loop must not go on to `WouldBlock`: the next read would wait for the user's next key. (A first version of this patch did, and the deck never drew its dashboard.) Upstream's `WouldBlock` arm stays in place but does not fire on a real terminal.

Unchanged on purpose: the read loop still has no break on `Ok(0)` while the parser is empty. A hung-up terminal spins there as it did upstream, and the deck's hangup watchdog (`src/terminal_hangup.rs`) ends the TUI from another thread.

## Why a patched copy

- No crossterm release fixes it. 0.29.0 is the newest on crates.io, and crossterm's `master` still returns from inside the batch loop (checked 2026-10-25).
- The `use-dev-tty` event source uses level-triggered `poll(2)` and so does not strand input, but its `try_read` never polls when the timeout is zero, so `event::poll(Duration::ZERO)` returns `false` unless an event is already queued. The deck relies on zero-timeout polls to drain input bursts and detect pastes.
- The deck cannot work around it from outside: crossterm's event source and parser are private, and the stranded bytes only reach crossterm through a new readiness edge.

## Checking it

`git diff --no-index --ignore-cr-at-eol` against the registry copy (`~/.cargo/registry/src/*/crossterm-0.29.0`) shows the whole change: the `try_read` loop and a test module at the end of `src/event/source/unix/mio.rs`; in `Cargo.toml`, the `[[example]]` entries removed (the examples, README, CHANGELOG and docs are not copied) and an empty `[workspace]` table added so cargo can run the crate on its own; a `.gitignore` for the lockfile and target directory that run writes; and this file. The copy is stored with LF line endings, which the repository's `.gitattributes` requires; upstream's sources are CRLF.

The test module reproduces both orders of the batch deterministically on Linux, over a blocking socket as the stand-in terminal: against upstream's loop the key behind a SIGWINCH is stranded, and a SIGWINCH behind a key is lost. No gate runs it, because the crate is outside the workspace; run it by hand after changing this copy:

```sh
cargo test --manifest-path vendor/crossterm/Cargo.toml --lib a_sigwinch_and_input_in_one_batch_are_both_delivered
```

It resolves the crate's own dev-dependencies, which the deck's `Cargo.lock` does not carry, so it needs the network the first time.

The deck-level check is `resize/sigwinch/004` (`tests/e2e_resize_input.rs`), which lane 1 runs.

## Dropping it

When a crossterm release handles the whole readiness batch, delete this directory and the `[patch.crates-io]` entry, and keep `resize/sigwinch/004`. Before any crossterm bump, read the root `Cargo.toml` comment on the `crossterm` pin.

## License

MIT, as upstream; see `LICENSE`.
