# Host metrics

PRD #1258 (complete; the PRD is `prds/done/1258-resource-aware-decks.md`). The daemon reports the machine it runs on (disk per watched role, load per core, memory) through one capability-gated attach verb, and judges each reading with a health level. The TUI's **Host of this deck** overlay (`m`) and the desktop Dashboard's per-daemon **Host** button and popover both read it. The user-facing page is [`docs/session-management.md`](../session-management.md#check-the-machine-a-deck-runs-on); this page covers the mechanism. The figures are the host's, never an agent's: why the deck does not attribute them to agents, with the measurements, is [per-agent resource attribution](per-agent-resources.md) (PRD #1692).

## The sampler

`src/host_metrics.rs` owns it. It is also the only place in the tree that reads the load average: `machine_load_per_cpu` used to exist twice, in `src/test_budget.rs` and in `tests/common/mod.rs`, and issue #1245 had to teach each copy macOS separately. Both now call this module (M2). A new caller should import from here rather than reading `/proc/loadavg` itself.

The reply's wire shape (`HostMetrics`, `DiskUsage`, the three `ROLE_*` names) and the client's size bounds (`MAX_DISK_ROLES`, `MAX_ROLE_BYTES`, `HostMetrics::within_reply_bounds`) live in `src/daemon_protocol.rs`, not here. Linkage-check rule 12 (`xtask/linkage-check/src/desktop_project_boundary.rs`) allows the desktop crate to name `daemon_protocol` and `daemon_client` but not `host_metrics`, which reads `/proc` and calls `statvfs`; keeping the types with the protocol lets the desktop decode and bound the reply without the boundary being widened to the sampler.

One sample (`sample_host`) is:

- a `statvfs` per watched role: `f_bavail × f_frsize` as free (what an unprivileged build can actually use) and `f_blocks × f_frsize` as total;
- the one-minute load average (`/proc/loadavg` on Linux, `getloadavg(3)` on macOS) divided by `available_parallelism()`, plus that core count;
- memory: on Linux from `/proc/meminfo`, `MemTotal − MemAvailable` as used and `MemAvailable` as available; on macOS, see below.

**Every reading degrades on its own.** A field the host cannot read is `None` on the wire (`#[serde(default, skip_serializing_if = "Option::is_none")]`), never `0`, so a client can tell "idle" or "full" from "unknown". Both clients render `None` as `unknown`. `protocol_host_metrics_004_unreadable_memory_is_absent_not_zero` pins this for memory.

**macOS memory** (round 2 of PR #1672; until then it was absent). The total is `sysctl hw.memsize`. Available memory is the total less what Activity Monitor calls "Memory Used" — App Memory + Wired + Compressed, App Memory being internal (anonymous) pages less purgeable ones — from `host_statistics64(HOST_VM_INFO64)`:

```text
available = total − (internal_page_count − purgeable_count + wire_count + compressor_page_count) × page_size
```

What remains is free memory plus Activity Monitor's "Cached Files" (file-backed and purgeable pages), which macOS reclaims before it compresses or swaps — the meaning Linux's `MemAvailable` has. Free + inactive pages, the other common recipe, undercounts, because macOS keeps much of its reclaimable file cache on the active queue. Used is the total less available. The result is clamped into `0..=total`, since the two readings are separate calls. Both figures need the total, so an unreadable `hw.memsize` or a failed `host_statistics64` leaves both `None`.

`page_size` is the kernel's, from `host_page_size`, not `libc::vm_page_size`, which is the process's: under Rosetta the release's `x86_64-apple-darwin` build sees 4 KiB process pages while the kernel counts in 16 KiB ones, which would put Memory Used 4× low (PR #1672 review, round 3). `libc` does not declare `host_page_size`, so `src/host_metrics.rs` does. A failed call, or a zero size, leaves both figures `None`. The translated case has not been run: `build-macos` runs the native build.

`mach_host_self` comes from `libc`, which marks it deprecated in favour of the `mach2` crate. The function is the system's and works; the lint only points at a crate with fuller Mach bindings. One call did not justify a new dependency, so `macos_vm_counts` allows the lint, and takes the host port once per process (a `OnceLock`) rather than adding a reference to it on every sample. The formula (`macos_available_bytes`) is a pure function whose unit test runs on every platform; `the_memory_is_measurable_on_macos` runs on `build-macos` only and is the one check that the Mach call answers on a real Mac in bytes, not pages. Nothing on a Linux box exercises the Mach call itself: `cargo check --target aarch64-apple-darwin` type-checks it against the `libc` declarations, which is as far as a Linux host goes.

## Health levels

Each reading the daemon can judge carries an `ok` / `warn` / `critical` level beside its number (`HealthLevel` in `src/daemon_protocol.rs`): `DiskUsage::level` per role, and `HostMetrics::load_level` and `memory_level`. They are additive optional fields on the existing reply — CLAUDE.md rule 18's additive rung — so there is no `PROTOCOL_VERSION` bump and no `CONTRACT_BREAKS` entry: an older client ignores them, and an older daemon omits them, which both clients show as an uncoloured number rather than `ok`. A level a client does not recognise deserializes as `None` (`deserialize_health_level`), so a newer daemon adding a level cannot make an older client refuse the reply.

`apply_health_levels` (`src/host_metrics.rs`) sets them from the numbers and `HealthThresholds`, and **clears** any whose number is absent: disk needs both figures and a non-zero total, memory needs used and available (the total is their sum), load needs a finite value. A value exactly at a threshold stays on the healthier side (`level_when_below` uses `<`, `level_when_above` uses `>`). The sample is judged on the sampler thread, so a fixed e2e sample (`DOT_AGENT_DECK_E2E_HOST_SAMPLE`) is judged too, and cached with its levels: a changed threshold shows from the next sample, at most `HOST_METRICS_MAX_AGE` later.

The thresholds are per deck, in `config.toml`'s `[host_metrics]` table, which `DashboardConfig::host_metrics` keeps **as written** (a `toml::Table`) — the precedent is PRD #1223's `default_dir`, the other deck-level setting the daemon reads, read with `DashboardConfig::load_bounded_reporting` on each sample. Parsing is per value in `HealthThresholds::from_table`: a value that is not a number in range (percentages `0..=100`, loads `>= 0`, both finite) falls back to its own default, an unknown key is ignored, and a pair whose critical threshold is less severe than its warning resets both, each with a warning. A `host_metrics` that is not a table reads as empty (a lenient deserializer), and the reporting loader raises a warning for it. None of this fails the rest of `config.toml`, which a strict `serde` struct would have. The loader returns its own warnings — a file it cannot read, malformed TOML, a setting of the wrong type, the non-table `host_metrics` — instead of logging them, and `configured_thresholds` logs them together with `from_table`'s only when that set changes from the previous sample's (`newly_raised`), so neither an open overlay polling every two seconds nor several desktops asking at once repeats them; a set that comes back after a clean read is logged again (`protocol/host-metrics/015`). `DashboardConfig::load_bounded`, which `NewAgentOptions` uses once per query, still logs on every read. `config set host_metrics.<key>` validates the same way and refuses an out-of-range value and one that would invert its pair (naming the other key to change first); `config get` prints the value in effect.

Clients never compute a level. The TUI colours a judged value with `palette::health_color` (named ANSI green / yellow / red, so a terminal theme remaps them) and both clients append `(warning)` or `(critical)` after a non-ok value (`host_metrics_level_word` / `hostLevelWord`), which the shared copy fixture pins. The desktop's button takes the worst level of the deck's readings (`worstHostLevel`) and uses the theme's green / amber / red families, which have light and dark values.

These are per-reading health levels, not a judgement of whether a deck can take another unit. That headroom verdict, which needs a unit's footprint, moved to PRD #1264.

## The three roles

The reply names roles, never paths, so a client learns nothing about the host's directory layout and has no path to derive anything from (PRD #819 / linkage-check rule 12: clients read no `/proc` and derive no path). The roles are fixed in this PRD, not configurable:

| Role | Path measured | Why this source |
|---|---|---|
| `working_root` | `project_resolve::daemon_startup_cwd()`, the daemon's cwd captured once by `run_daemon_with` | The daemon has no other single project root: agent cwds are per pane and `working_dir` belongs to schedules. Capturing it once means the answer does not move if the process cwd does. A harness server that never captured it falls back to `current_dir()`. |
| `worktree_parent` | the parent of `working_root` | where `../<repo>-dispatch-*` sibling worktrees land |
| `temp_root` | `DAD_E2E_TMPDIR` if set and non-empty, else `/var/tmp/dad-e2e-<uid>` on Unix, else the OS temp dir | the same rungs `config_write_guard::default_test_roots` names for the e2e harness's standard temp root ([e2e temp directories](e2e-temp-dirs.md)) |

A role whose path does not exist is measured at its nearest existing ancestor, which is the filesystem the directory would be created on. The harness's validation of a moved temp base is not repeated, because measuring a filesystem needs no trust.

## Cache, max age and cost

There is no timer. `HostMetricsCache` samples **on demand** and reuses a sample for `HOST_METRICS_MAX_AGE` (2 s). There is one cache per attach server (`serve_attach_with_restart`), passed to `handle_connection`. Ages are read on Tokio's clock, so the caching test (`protocol/host-metrics/002`) is event-sequenced with a paused clock rather than sleep-based. Every reply carries `sampled_at_ms` (wall clock) and `sample_age_ms`, so a client never has to guess how stale a cache hit is.

**Sampling is single-flight, and decided on the async side** (security audit A1). A `statvfs` on a hung NFS or FUSE mount never returns, and no timeout cancels a syscall that is already running. The first version started a `spawn_blocking` job per request and took a synchronous mutex inside it, held across the `statvfs` calls, so a hung mount left one job stuck in the syscall and every later one stuck on the mutex, until Tokio's shared blocking pool was full and unrelated spawn, close and shutdown work queued behind it. `HostMetricsCache::read` now:

- answers a request inside the max age from the cache, with no blocking job;
- starts a refresh only when the sample is stale **and** none is in flight; that refresh is the only `spawn_blocking` the verb ever makes;
- answers a request that finds a refresh in flight at once from the last sample, at its true age, which may be past the max age;
- makes a request with no last sample wait for the refresh for at most `HOST_METRICS_SAMPLE_WAIT` (1 s, under the desktop's 2 s reply timeout), then answer with what the cache holds: the generic error `host-metrics: no host sample is available yet` if that is nothing.

A refresh that never finishes therefore holds one blocking thread, however many requests arrive, and every request still gets an answer within the wait. Its in-flight marker is cleared by a drop guard, so a refresh that panics or fails leaves the cache able to start the next one; a failed refresh keeps the last sample, which keeps aging honestly. The cache's own lock guards two fields and is never held across an `await`, a spawn or a syscall. A landed sample is aged from when it landed, not from when its refresh started. The sampler is injected (`with_sampler`), and `protocol/host-metrics/005` drives it with a sample that never finishes: eight requests on a cold cache start one sample and all answer once the wait passes; after a sample lands and expires, eight more start one more and the seven that did not start it answer without waiting, with the last sample's true age.

Fixing this with a timeout on the client or the server alone would not have worked, for the reason above: the job keeps running. Bounding a hung sampler's *lifetime*, rather than how many there are, would need the probes in a separately managed worker or process outside Tokio's pool. One stuck thread was judged an acceptable cost for not doing that.

Measured for M1 in a debug build on the 16-core dev box at load average ~25–29, 200 iterations each:

| | median | p95 | max |
|---|---|---|---|
| cold sample (three `statvfs` plus `/proc/loadavg` and `/proc/meminfo`) | 105 µs | 262 µs | 71.7 ms (one scheduling outlier under load) |
| cache hit (within the max age; measured before audit A1 moved the hit to the async side) | 221 ns | 250 ns | |

## The verb and its gate

`AttachRequest::HostMetrics` (a unit variant) answers with the additive optional `AttachResponse::host_metrics`. It is gated on `CAP_HOST_METRICS = "host-metrics"`, which is on the Unix `DAEMON_CAPABILITIES` list only: a Windows daemon does not advertise it and refuses the verb if a raw sender skips the check. The check lives in the client library, `DaemonClient::host_metrics()`, which answers `HostMetricsReport::NotAvailable` without sending a frame when the capability is absent (`protocol/host-metrics/003`). That is rule 18's gated-variant rung, so there is no `PROTOCOL_VERSION` bump and no `CONTRACT_BREAKS` entry. The residual case is a cached capability set that outlives a daemon replaced by an older build. It fails closed: the older daemon refuses the unknown variant and the client returns `ClientError::Server`.

**The client bounds the reply's size** (security audit A2). The attach frame ceiling is 16 MiB, so a hostile or broken daemon could send tens of thousands of roles: 65,525 one-letter roles fit in 852 KB, and the TUI's overlay height (`lines.len() as u16 + 5`) then overflowed `u16`. `DaemonClient::host_metrics()` returns `ClientError::Malformed("host-metrics reply exceeded its size bounds")` for a reply with more than `MAX_DISK_ROLES` (16) roles or a role longer than `MAX_ROLE_BYTES` (64 bytes); the daemon sends three roles of under twenty bytes, so the headroom is for roles a newer daemon adds. The message names neither figure, because the peer chose both. Role text inside the bounds passes through unchanged (`protocol/host-metrics/007`–`009`). The renderers then bound their own work as defence in depth: the TUI draws at most `MAX_DISK_ROLES` rows, computes the overlay height in `usize` with saturating arithmetic and clamps it before narrowing, and shows a role it does not know through `untrusted_text::display_line` (control and bidi characters stripped, clamped to the 18-column label) (`dashboard/host-metrics/005`–`007`); the desktop's `HostMetricsReportDto::from_report` keeps at most `MAX_DISK_ROLES` roles, each clamped to `MAX_ROLE_BYTES`, and the panel's React keys go through `domIdentity`.

`NotAvailable` is an outcome, not an error. Both clients show it as "Host metrics are not available from this deck" (rule 22: same words), never as zeros.

**For PRD #1264's verdict:** the tests pin `AttachRequest::HostMetrics` as a unit variant. If the headroom verdict (moved there from this PRD's former M5) takes its optional footprint argument on this verb, it becomes a struct variant with `#[serde(default)]` fields, and that means updating the tests' `AttachRequest::HostMetrics` expressions.

## The same words in both clients

The overlay and the desktop popover use the same title, subtitle, row labels, role labels, formats and level words (rule 22). `tests/fixtures/host-metrics-copy.json` holds them once: samples in the daemon's wire shape with the label/value rows each must render as, plus the not-available sentences. The TUI's `dashboard_host_metrics_008_words_match_the_shared_copy` (`tests/render_host_metrics_copy.rs`) renders the overlay for each sample, and the desktop's `HostMetricsPanel.copy.test.tsx` renders the popover's panel (and checks each row's level against the fixture's `levels`), so a word changed in one client alone fails the other's test, the pattern `tests/fixtures/editing-shortcuts.json` set for PR #1429.

## How each client refreshes

**TUI.** The `host_metrics` action (default `m`, `[dashboard]` in `keybindings.toml`) dispatches `Action::OpenHostMetrics`, which switches to `UiMode::HostMetrics` and asks through `EmbeddedPaneController::host_metrics_reader()`. `HostMetricsReader::request` spawns the query on the runtime and returns at once, so the UI thread never waits on the socket; at most one request is in flight. `poll_host_metrics` runs once per pass of the existing event loop (the loop that already wakes every frame to drain input). It lands the answer and, while the overlay is open, asks again once the shown answer is older than `HOST_METRICS_MAX_AGE`, because asking sooner would only return the daemon's cached sample. It does nothing while the overlay is closed and does not re-ask a `NotAvailable` deck. There is no new timer. The L1 seams `render_host_metrics_overlay_to_buffer` and `render_host_metrics_key_sequence_to_buffers` drive the live renderer and the live `handle_key_event` → `dispatch_action` path with an injected answer and no daemon.

**Desktop.** The bridge asks each connected daemon through the same `DaemonClient::host_metrics()` on the `TrustedDaemon`'s client, whose capability cache the handshake seeded, so an older daemon costs no connection. The request rides the existing refresh paths, concurrently with the `ListAgents` it accompanies (`tokio::join!`):

- every non-watcher snapshot (`desktop_bootstrap`, `desktop_get_snapshot`, the `refresh_and_emit` after each action) fetches it fresh;
- the per-deck watcher fetches it whenever its `AgentView` fetches the agent list: the first refresh, every `RECONCILE_INTERVAL` (5 s) reconcile tick, and every refetch nudge. Folded-event refreshes replay the held answer from the view, so a burst of events costs no host-metrics connection.

The webview shows the answer behind a **Host** button in each deck's header (`HostMetricsButton` in `desktop/src/components/HostMetricsPanel.tsx`), which replaced a panel under the header in round 2 because the panel took too much of the overview. The popover is dismissed like the dashboard's Filter menu: a `pointerdown` outside its root, and `Escape` from anywhere, which also returns focus to the button.

Which popover is open is the overview's state (`hostOpen` in `AgentOverview.tsx`, one deck id at most), not the button's: the button's click and the voice row `show_host_metrics` (PR #1672) both dispatch the registry entry `VOICE_ACTIONS.showHostMetrics`, and `chooseHostDeck` (`desktop/src/lib/hostMetrics.ts`) decides which deck a request without one means. [`voice-first-design.md`](voice-first-design.md) section 3 has the voice side.

A held answer's age is the daemon's `sample_age_ms` plus the time the bridge has held it, so a replayed answer does not claim to be as fresh as when it arrived. Neither client shows the age since PR #1672's Mac test (the TUI's count climbed and reset every couple of seconds, the desktop's read `0`, and the user found the row useless, with no stale warning in its place); `sampled_at_ms` and `sample_age_ms` stay on the wire, in the daemon's cache and in the bridge's DTO for PRD #1264's placement work. A failed or slow request (bounded at `HOST_METRICS_REPLY_TIMEOUT`, 2 s) yields no `hostMetrics` on that snapshot and never invalidates the link. The webview then keeps the deck's last answer while the deck stays connected, and drops it when the deck disconnects. The worst-case staleness on a quiet deck is the reconcile interval plus the daemon's 2 s cache.
