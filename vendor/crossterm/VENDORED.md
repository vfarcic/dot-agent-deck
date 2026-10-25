# crossterm 0.29.0, patched

This directory is crossterm 0.29.0 as published on crates.io (upstream commit `36d95b26a26e64b0f8c12edfe11f410a6d56a812`), with one code change, in `src/event/source/unix/mio.rs` ("Checking it" lists the rest). The root `Cargo.toml` puts it in place of the registry crate with `[patch.crates-io]`, so both the deck and `ratatui-crossterm` build against it. It is excluded from the workspace, so the workspace's fmt and clippy gates do not run on it.

## What the patch fixes

A key typed right after a terminal resize could sit unread until the user typed something else. The TUI looked frozen to that key; the next keystroke released it.

The mio event source handles one batch of readiness events per `poll`. Upstream returned from inside that loop as soon as it met the SIGWINCH token, or as soon as the terminal token yielded one parsed event. Any token later in the same batch was never handled, and mio's readiness is edge-triggered (epoll `EPOLLET` on Linux, kqueue `EV_CLEAR` on macOS), so it was not reported again: unread terminal bytes stayed unread until new input raised a new edge, and a SIGWINCH queued behind input waited for the next resize. A batch holds both when a resize and a keystroke arrive while the event loop is busy, for example while it is drawing the frame for the resize before.

The patched loop handles every token in the batch first: for the terminal it reads as upstream did and stops reading where upstream returned, once those reads have parsed an event (an event an earlier token queued, such as a `Resize`, does not count, or a paste longer than one read would be cut off after its first read and the rest left unread); for a SIGWINCH it queues a `Resize` in the parser's event queue in batch order; then it returns the first queued event. The next call returns the rest before it polls again. An error from measuring the terminal or a waker interrupt is returned after the batch rather than in the middle of it, so input read in the same batch stays queued for the next call.

The terminal descriptor is blocking, so the read loop must not go on to `WouldBlock`: the next read would wait for the user's next key. (A first version of this patch did, and the deck never drew its dashboard.) Upstream's `WouldBlock` arm stays in place but does not fire on a real terminal.

Unchanged on purpose: the read loop still has no break on `Ok(0)` while the parser is empty. A hung-up terminal spins there as it did upstream, and the deck's hangup watchdog (`src/terminal_hangup.rs`) ends the TUI from another thread.

## Why a patched copy

- No crossterm release fixes it. 0.29.0 is the newest on crates.io, and crossterm's `master` still returns from inside the batch loop (checked 2026-10-25).
- The `use-dev-tty` event source uses level-triggered `poll(2)` and so does not strand input, but its `try_read` never polls when the timeout is zero, so `event::poll(Duration::ZERO)` returns `false` unless an event is already queued. The deck relies on zero-timeout polls to drain input bursts and detect pastes.
- The deck cannot work around it from outside: crossterm's event source and parser are private, and the stranded bytes only reach crossterm through a new readiness edge.

## Checking it

`git diff --no-index --ignore-cr-at-eol` against the registry copy (`~/.cargo/registry/src/*/crossterm-0.29.0`) shows the whole change: in `src/event/source/unix/mio.rs`, the `try_read` loop, a `terminal_size` function the SIGWINCH arm measures through (the tests give it a size or a failure) and a test module at the end; in `Cargo.toml`, the `[[example]]` entries removed (the examples, README, CHANGELOG and docs are not copied) and an empty `[workspace]` table added so cargo can run the crate on its own; a `Cargo.lock` for the crate's own tests, and a `.gitignore` for the target directory they write; and this file. The copy is stored with LF line endings, which the repository's `.gitattributes` requires; upstream's sources are CRLF.

The test module reproduces the batch cases on Linux, over a blocking socket as the stand-in terminal, and checks the exact events delivered, in order and once each: a key behind a SIGWINCH (upstream stranded it), a SIGWINCH behind a key (upstream lost it), a paste longer than two reads behind a SIGWINCH (an earlier version of this patch stranded its tail), and a failed measurement with a key in the same batch (the key still arrives, after the error). CI runs it in the `test` job on GitHub Actions and the `vendored-crossterm-test` task on RWX, `--locked` to this directory's `Cargo.lock`, which pins the dev-dependencies the deck's lock does not carry. Run it by hand after changing this copy:

```sh
cargo test --locked --manifest-path vendor/crossterm/Cargo.toml --lib event::source::unix::mio::tests
```

CI runs that module and not upstream's whole suite: two of upstream's colour tests (`test_parse_ansi_fg`, `test_parse_ansi_bg`) read `NO_COLOR` while another test in the same process sets it, and failed in one of a dozen local runs of the suite.

The deck-level check is `resize/sigwinch/004` (`tests/e2e_resize_input.rs`), which lane 1 runs.

## Keeping it applied and audited

While this copy exists, two checks stand in for what the registry crate got for free:

- **That the patch applies.** `[patch.crates-io]` applies only while this copy's version satisfies what the dependents ask for. When it does not, after a crossterm bump for example, cargo uses the registry crate and only prints a warning. `tests/vendored_crossterm.rs` (in `cargo test-fast`) fails when `Cargo.lock`'s crossterm entry is not this copy, so a Renovate bump PR goes red until the patch is re-applied to the new version or dropped.
- **Advisories for the upstream version.** `cargo audit` skips lock entries without a crates.io source (checked in cargo-audit 0.22.2, which CI installed when this was written), so the ordinary scan no longer covers crossterm. `scripts/audit-vendored-upstream.sh`, which CI runs next to `cargo audit` (a step in the `security` job on GitHub Actions, the `security-vendored-upstream` task on RWX), audits a copy of `Cargo.lock` that gives this crate its crates.io source back. That checks RustSec's advisories against upstream crossterm 0.29.0. It does not authenticate this copy, and an advisory it reports has to be read against the patch: fix it here, take the upstream release and re-apply or drop the patch, or record why it does not apply.

The obligation that goes with the copy: when crossterm publishes a release or an advisory, read it against this directory, as nothing updates the copy on its own.

## Dropping it

When a crossterm release handles the whole readiness batch, delete this directory, the `[patch.crates-io]` entry, `tests/vendored_crossterm.rs`, `scripts/audit-vendored-upstream.sh` and the CI steps that run the script and this crate's test, and keep `resize/sigwinch/004`. Before any crossterm bump, read the root `Cargo.toml` comment on the `crossterm` pin.

## License

MIT, as upstream; see `LICENSE`.
