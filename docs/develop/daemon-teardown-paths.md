# Daemon teardown paths: which ones refuse, which ones only disclose

There are four ways to ask a `dot-agent-deck` daemon to stop. Two of them can be refused and two cannot, and issue #1109 asked whether that asymmetry is a defect. **It is not, and this file is the decision.** The unrefusable paths stay unrefusable; what they gained is the obligation to say what they destroyed.

| how you ask | what runs in the daemon | guard | disclosure |
| --- | --- | --- | --- |
| `dot-agent-deck daemon stop` (and `daemon restart`) | `daemon_stop::run_daemon_stop` decides, then delivers a `SIGTERM` | issue #770 refusal, `--force` to override | the refusal, on stderr — and, once it does signal, the inventory below |
| `AttachRequest::StopDaemon` | the wire verb's arm in `daemon_protocol.rs` (#1049) | the same refusal, carried back as `StopDaemonRefusal` | the refusal, over the wire |
| a termination SIGNAL (`SIGTERM`/`SIGINT`, `CTRL_C` on Windows) | `daemon::spawn_termination_signal_watch` | **none, deliberately** | `daemon_stop::log_teardown_inventory`, at `warn!` |
| the `KIND_SHUTDOWN` frame (the TUI's Ctrl+C → `Stop`) | `daemon_protocol::handle_connection` | **none, deliberately** | the same call |

Note the first row and the third are the same handler at the daemon end: `daemon stop` refuses in the *client*, before it signals, so `--force` — the documented way to abandon a run — now leaves a record of what it abandoned.

Three further things reach that same graceful-shutdown signal with nobody asking, and none of them is in scope here. The idle-shutdown timer fires only with no clients, no agents and no pending schedules, so it has nothing to disclose. The orphan watchdog and the max-lifetime backstop are env-gated test safety nets, off in production. (A crash, an `OOM` kill or a `SIGKILL` ends a daemon too, of course, and reaches no code of ours at all.)

## What is actually at stake

The refusal exists because a daemon holds state that exists nowhere else. `pane_role_map`, `pane_orchestration_map`, `orchestrator_pane_ids` and `pane_cwd_map` are populated by `AppState::register_orchestration_role` and have no persistence path of any kind. Stopping the daemon deletes them, and an agent that has detached from the PTY it was born under survives the stop: it keeps running, keeps posting hook events, keeps looking healthy on its card — and can never delegate again. That is issue #770, and `CLAUDE.md` rule 15 carries the operational half.

Losing the agent *processes* is bad and recoverable. Losing the *registrations* is bad and not.

## Why the signal path must not refuse

`SIGTERM` is not a request from a peer. It is how a service manager, a container runtime, a session logout or a system shutdown asks a daemon to stop, and every one of those senders is running its own clock:

- `systemd` escalates to `SIGKILL` after `TimeoutStopSec`. A daemon that refuses does not survive; it dies *worse* — no graceful drain, so its agents are orphaned rather than reaped, and no disclosure either, because the process is gone before it can write one.
- A container runtime does the same on `docker stop`.
- Refusing to die is a classic way to make a machine hard to shut down, and the operator's next move is `kill -9`, which has the same outcome as the timeout above.

So a refusal on the signal path does not convert a destructive stop into a survivable one. It converts a *graceful* destructive stop into an *ungraceful* one, and loses the only chance the daemon has to leave a record. The refusal belongs where the caller can act on it — a CLI that can print it and exit non-zero, a wire verb whose client can present it — and nowhere else.

The same reasoning covers the escape hatch already in the signal handler: a second signal force-exits with status 143. That is deliberately kept, because installing a handler replaces the default disposition process-wide, and without it a wedged shutdown could no longer be ended with `pkill` (`lifecycle/sigterm/002`).

## Why `KIND_SHUTDOWN` does not refuse either

That frame is the `Stop` option of the TUI's Ctrl+C dialog: a user on the same machine who has already made this decision in front of the pane list. Adding a refusal there would mean refusing a person who just chose, which is a dialog problem rather than a daemon problem. It gets the disclosure for the same reason the signal path does — the registrations die with the process whoever asked — and `AttachRequest::StopDaemon` remains the guarded wire verb for every caller that is not that dialog.

What this does **not** claim: the TUI's secondary confirmation names an agent *count* (`Action::StopConfirmPrompt { agent_count }`) and does not name the orchestration roles at stake. Surfacing those in the dialog is a genuine improvement and a TUI change; it is out of scope here and not implemented.

## What was implemented instead: disclosure

`daemon_stop::log_teardown_inventory` runs on both unguarded paths, **before** the registry drain, and logs one `warn!` line naming:

- which teardown path emitted it (`signal`, `shutdown-frame`);
- how many managed agents are being terminated and how many role registrations destroyed;
- every live agent, as `id pane=… label=… cwd=…`;
- every orchestration role, rendered by the *same* helper the #770 refusal uses, so the two cannot describe one role differently;
- the permanence sentence — these registrations are held in memory only, so any agent that survives keeps running but can never delegate again.

Three properties are load-bearing:

1. **Before the drain.** `AgentPtyRegistry::agent_records` filters to live agents and `AppState::live_orchestration_roles` filters roles by `has_live_pane`, so the same call after `shutdown_all_graceful` reports an empty deck no matter what was running. `lifecycle/teardown-inventory/002` is what makes reordering the call sites fail instead of silently emptying the inventory.
2. **One line.** It is read by `grep` after the fact rather than by a human watching a terminal, and a daemon tearing down logs from several tasks at once. The guarded path's refusal is the multi-line one, and it is read live.
3. **Bounded.** The role half needs the state lock; `log_teardown_inventory` gives it `TEARDOWN_INVENTORY_LOCK_BUDGET` (200 ms) and, on expiry, logs that the roles are *unlisted* rather than absent. The number is not picked round. The binding clock is not the external sender's — those are tens of seconds — but our own: `daemon stop` sends the very SIGTERM this fires on and then polls `DAEMON_STOP_POLL_BUDGET` (5 s) for the daemon to go away, and the teardown that follows already spends `AGENT_TERMINATE_GRACE` (3 s) plus `FORCE_REAP_DEADLINE` (1 s) of it. The disclosure is charged to that same window, ahead of both, so it comes out of the 1 s of headroom that pays for unwinding, dropping the registry, exiting, and a client that samples only every 100 ms. A `const _: () = assert!(…)` in `daemon_stop.rs` pins the sum below the budget — raise the disclosure's share far enough and the build fails rather than turning a clean stop into a `TimedOut`.

## The two shapes that were rejected

**Distinguish the sender.** A `SIGTERM` from PID 1 during system shutdown is a different event from one from a sibling process, and the daemon could in principle treat them differently. Rejected on two grounds. First, the discrimination is not available where it would have to be read: tokio's signal API surfaces no `siginfo`, so this needs a hand-rolled `SA_SIGINFO` handler, and `si_pid` is `0` for kernel-generated signals and names a pid that may already have been reused. Second, and decisively, **the discrimination points the wrong way.** The only action a "sibling" classification could justify is refusing — and a sibling is exactly the `pkill` case from #428's occurrence #5, i.e. an operator or an agent at a shell, who on being refused reaches for `-9`. That reproduces the escalation this document rejects, against the one sender who was going to disclose nothing anyway. Real machinery, no defensible behaviour change.

**Make the daemon's command line not match a naive pattern.** `pkill -f "daemon serve"` matched the production daemon because `daemon serve` is what it runs. Obfuscating that would raise the bar for exactly one careless pattern while breaking `ps`, monitoring greps and operator muscle memory, and doing nothing for a deliberate signal — the next pattern anyone reaches for matches whatever the new name is. It trades a discoverable, documented subcommand for a speed bump.

**Doing nothing at all** was the fourth option the issue listed, and it is the one this decision is closest to — the *behaviour* is unchanged, on purpose. What it leaves on the table is the cost that was actually measured: after occurrence #5, establishing that nine panes across three dispatched units had been stopped took log archaeology, because the shutdown line said "every managed agent will be stopped" and named none of them. That is cheap to fix — it refuses nothing, and the shutdown latency it can add is bounded at 200 ms and pinned by the assertion above — and it pays off in every future incident, so "do nothing" was rejected in favour of "change nothing except what the daemon says".

## What this does not close

- The disclosure is a record, not a prevention. A stray `SIGTERM` still takes the daemon and the agents it manages down; you will simply know which ones they were. Prevention for the specific cause that prompted #1109 — cross-version sandbox teardown with an unscoped `pkill` — is `CLAUDE.md` rule 12's teardown step.
- #428's "Still open" item 1, the e2e harness's unisolated attach socket, is a separate live problem and is untouched here.
- Nothing here identifies a `SIGTERM` sender. The inventory says what was lost, not who asked.
