# Host metrics

PRD #1258 M1–M4. The daemon reports the machine it runs on (disk per watched role, load per core, memory) through one capability-gated attach verb. The TUI's **Host of this deck** overlay (`m`) and the desktop Dashboard's per-daemon panel both read it. The user-facing page is [`docs/session-management.md`](../session-management.md#check-the-machine-a-deck-runs-on); this page covers the mechanism.

## The sampler

`src/host_metrics.rs` owns it. It is also the only place in the tree that reads the load average: `machine_load_per_cpu` used to exist twice, in `src/test_budget.rs` and in `tests/common/mod.rs`, and issue #1245 had to teach each copy macOS separately. Both now call this module (M2). A new caller should import from here rather than reading `/proc/loadavg` itself.

One sample (`sample_host`) is:

- a `statvfs` per watched role: `f_bavail × f_frsize` as free (what an unprivileged build can actually use) and `f_blocks × f_frsize` as total;
- the one-minute load average (`/proc/loadavg` on Linux, `getloadavg(3)` on macOS) divided by `available_parallelism()`, plus that core count;
- memory from `/proc/meminfo` on Linux: `MemTotal − MemAvailable` as used, `MemAvailable` as available.

**Every reading degrades on its own.** A field the host cannot read is `None` on the wire (`#[serde(default, skip_serializing_if = "Option::is_none")]`), never `0`, so a client can tell "idle" or "full" from "unknown". Both clients render `None` as `unknown`. `host_metrics_004_unreadable_memory_is_absent_not_zero` pins this for memory.

**macOS memory is absent, deliberately.** The Mach host-statistics calls (`host_statistics64`) are marked deprecated in the `libc` crate in favour of the `mach2` crate, which this project does not depend on. Adding a dependency for one informational figure was not worth it, so a macOS deck reports memory as `unknown`. Disk and load work there.

## The three roles

The reply names roles, never paths, so a client learns nothing about the host's directory layout and has no path to derive anything from (PRD #819 / linkage-check rule 12: clients read no `/proc` and derive no path). The roles are fixed in this PRD, not configurable:

| Role | Path measured | Why this source |
|---|---|---|
| `working_root` | `project_resolve::daemon_startup_cwd()`, the daemon's cwd captured once by `run_daemon_with` | The daemon has no other single project root: agent cwds are per pane and `working_dir` belongs to schedules. Capturing it once means the answer does not move if the process cwd does. A harness server that never captured it falls back to `current_dir()`. |
| `worktree_parent` | the parent of `working_root` | where `../<repo>-dispatch-*` sibling worktrees land |
| `temp_root` | `DAD_E2E_TMPDIR` if set and non-empty, else `/var/tmp/dad-e2e-<uid>` on Unix, else the OS temp dir | the same rungs `config_write_guard::default_test_roots` names for the e2e harness's standard temp root ([e2e temp directories](e2e-temp-dirs.md)) |

A role whose path does not exist is measured at its nearest existing ancestor, which is the filesystem the directory would be created on. The harness's validation of a moved temp base is not repeated, because measuring a filesystem needs no trust.

## Cache, max age and cost

There is no timer. `HostMetricsCache` samples **on demand** and reuses a sample for `HOST_METRICS_MAX_AGE` (2 s). There is one cache per attach server (`serve_attach_with_restart`), passed to `handle_connection`. The sample runs in `spawn_blocking` and the age is read on Tokio's clock, so the caching test (`protocol/host-metrics/002`) is event-sequenced with a paused clock rather than sleep-based. Every reply carries `sampled_at_ms` (wall clock) and `sample_age_ms`, so a client never has to guess how stale a cache hit is.

Measured for M1 in a debug build on the 16-core dev box at load average ~25–29, 200 iterations each:

| | median | p95 | max |
|---|---|---|---|
| cold sample (three `statvfs` plus `/proc/loadavg` and `/proc/meminfo`) | 105 µs | 262 µs | 71.7 ms (one scheduling outlier under load) |
| cache hit (`read_at` within the max age) | 221 ns | 250 ns | |

## The verb and its gate

`AttachRequest::HostMetrics` (a unit variant) answers with the additive optional `AttachResponse::host_metrics`. It is gated on `CAP_HOST_METRICS = "host-metrics"`, which is on the Unix `DAEMON_CAPABILITIES` list only: a Windows daemon does not advertise it and refuses the verb if a raw sender skips the check. The check lives in the client library, `DaemonClient::host_metrics()`, which answers `HostMetricsReport::NotAvailable` without sending a frame when the capability is absent (`protocol/host-metrics/003`). That is rule 18's gated-variant rung, so there is no `PROTOCOL_VERSION` bump and no `CONTRACT_BREAKS` entry. The residual case is a cached capability set that outlives a daemon replaced by an older build. It fails closed: the older daemon refuses the unknown variant and the client returns `ClientError::Server`.

`NotAvailable` is an outcome, not an error. Both clients show it as "Host metrics are not available from this deck" (rule 22: same words), never as zeros.

**For M5:** the tests pin `AttachRequest::HostMetrics` as a unit variant. M5's optional footprint argument will make it a struct variant with `#[serde(default)]` fields, and that means updating the tests' `AttachRequest::HostMetrics` expressions.

## How each client refreshes

**TUI.** The `host_metrics` action (default `m`, `[dashboard]` in `keybindings.toml`) dispatches `Action::OpenHostMetrics`, which switches to `UiMode::HostMetrics` and asks through `EmbeddedPaneController::host_metrics_reader()`. `HostMetricsReader::request` spawns the query on the runtime and returns at once, so the UI thread never waits on the socket; at most one request is in flight. `poll_host_metrics` runs once per pass of the existing event loop (the loop that already wakes every frame to drain input). It lands the answer and, while the overlay is open, asks again once the shown answer is older than `HOST_METRICS_MAX_AGE`, because asking sooner would only return the daemon's cached sample. It does nothing while the overlay is closed and does not re-ask a `NotAvailable` deck. There is no new timer. The L1 seams `render_host_metrics_overlay_to_buffer` and `render_host_metrics_key_sequence_to_buffers` drive the live renderer and the live `handle_key_event` → `dispatch_action` path with an injected answer and no daemon.

**Desktop.** The bridge asks each connected daemon through the same `DaemonClient::host_metrics()` on the `TrustedDaemon`'s client, whose capability cache the handshake seeded, so an older daemon costs no connection. The request rides the existing refresh paths, concurrently with the `ListAgents` it accompanies (`tokio::join!`):

- every non-watcher snapshot (`desktop_bootstrap`, `desktop_get_snapshot`, the `refresh_and_emit` after each action) fetches it fresh;
- the per-deck watcher fetches it whenever its `AgentView` fetches the agent list: the first refresh, every `RECONCILE_INTERVAL` (5 s) reconcile tick, and every refetch nudge. Folded-event refreshes replay the held answer from the view, so a burst of events costs no host-metrics connection.

A held answer's age is the daemon's `sample_age_ms` plus the time the bridge has held it, so a replayed answer does not claim to be as fresh as when it arrived. A failed or slow request (bounded at `HOST_METRICS_REPLY_TIMEOUT`, 2 s) yields no `hostMetrics` on that snapshot and never invalidates the link. The webview then keeps the deck's last answer while the deck stays connected, and drops it when the deck disconnects. The worst-case staleness on a quiet deck is the reconcile interval plus the daemon's 2 s cache.
