# Draft deferral: how an automatic prompt waits for an unsent draft

Maintainer notes for issue #544. The user-facing behaviour is described in [Orchestration → A deck prompt waits while you have an unsent draft](../orchestration.md#a-deck-prompt-waits-while-you-have-an-unsent-draft); this page carries the mechanism, its bounds and the gaps that the user page deliberately leaves out. The code is `src/draft_deferral.rs` (the gate and the draft bit), `src/pane_delivery_queue.rs` (per-pane ordering), and the guarded first-write entries in `src/agent_pty.rs`.

## The problem

Every daemon-originated first write — a delegate's task pointer, a `work-done` hand-off, a dispatch result, a scheduled or spawn-time seed, a reuse fire, the deck's own worker reports — is payload plus CR. Written into an input box that already holds a half-typed user message, that CR submits both as one turn, for example `half a thoughtRead .dot-agent-deck/worker-task-coder.md for your task.` Issue #424 closed the retry half of this (a repeat of bytes already written is refused once the user has typed since); issue #544 is the first-write half.

## What counts as a draft, and how it is judged

The daemon cannot see an agent's input box, but it does see every byte a deck client forwards into it. It keeps one bit per pane: "the user has sent input into this pane since their last submit or clear". A first write consults that bit and, while it is set, waits — re-checking every 200 ms (`DRAFT_POLL_INTERVAL`) — until the user submits or clears, or until the cap passes. At the cap it writes exactly as it did before #544 and marks the pane's session **Error**. It never refuses and never drops, which is what lets it coexist with #424's "a seed prompt must arrive" constraint.

- **Sets the bit:** any forwarded byte that is not part of a recognised terminal report — printable text and UTF-8, arrows and other navigation keys (editing a history-recalled prompt is editing a draft), Backspace, Tab, `Ctrl+J`, `Alt+Enter`, `Shift+Enter`, and anything inside a bracketed paste (including a literal `ESC[200~` pasted as text).
- **Clears the bit:** a byte that submits the input box, and — outside a paste — `Ctrl+U` and `Ctrl+C`.
- **Neither:** terminal reports a client forwards through the same channel as keystrokes (mouse, focus, cursor-position, device-attribute and similar replies, OSC/DCS strings) and the bracketed-paste markers themselves. One ambiguity is accepted: xterm encodes a modified `F3` as `ESC[1;5R`, the shape of a cursor-position report, so that key does not set the bit.

## What is not detected

The bit is a proxy, and it is wrong in both directions in ways that are bounded:

- **Missed drafts (the old behaviour returns).** Text the agent itself puts into its input box — a prompt recalled from history, an autocompletion, a restored message — is never seen as typed. Neither is anything typed into the agent other than through the deck. A partial clear read as a full one lets a draft through: `Ctrl+U` in a multi-line Claude Code draft kills only the current line, but clears the bit.
- **False drafts (a delay of at most the cap).** A key that leaves nothing in the box still sets the bit — answering a menu with a number and no Enter, or deleting a draft back to empty with Backspace. The prompt then waits until the user's next Enter or the cap.

An approximation in the clear keys can only let a draft through as before; it can never hold a prompt past the cap.

## Prompts that do not wait

Only daemon-originated automatic first writes are gated. The TUI/desktop `WriteAndSubmit` RPC deliberately stays on the immediate write path: the TUI calls it from its UI thread, and the desktop's `SubmitText` is the user's own submit. So prompts the TUI or the desktop app send on the user's behalf — such as a new orchestration's first prompt to its orchestrator — do not wait. Empty payloads (submit-only probes) are not gated either; #424 already refuses those once the user has typed.

## The cap

`DOT_AGENT_DECK_DRAFT_DEFER_CAP_MS` sets the cap in milliseconds. The default is 60 seconds (`DEFAULT_DRAFT_DEFER_CAP`) — the same bound as the automatic-prompt deadline and the scheduler's reuse hard timeout. `0` switches the gate off and restores the pre-#544 immediate write. A value above ten minutes (`MAX_DRAFT_DEFER_CAP`) is clamped to ten minutes, because past that an orchestration waiting on a draft nobody is finishing is stalled rather than deferred; a non-numeric value falls back to the default with a warning. The value is read once, when the daemon's PTY registry is constructed, so it must be set on the daemon's environment (the process that starts the deck, when the daemon is lazy-spawned), and a daemon that is already running keeps its value until it restarts.

**A scheduled task's reuse fire shares one budget with its typing debounce.** The reuse path's own limit is one minute (`REUSE_DELIVERY_HARD_TIMEOUT`), counted from the start of the debounce, and the draft cap is clamped to it — so the two waits together stay within that minute rather than stacking, whatever the environment variable says. The variable can therefore shorten a reuse fire's draft wait but not lengthen it. A newly opened tab's first prompt is not a reuse fire: if the user starts typing into it before the prompt arrives, it waits for up to the full cap. The spawn seed's own deadline is shifted by the time spent waiting on the draft, so a draft wait does not eat the seed's delivery allowance.

## Where the cause shows up

At the cap the daemon publishes a synthetic `Error` event for the pane, carrying `DRAFT_CAP_NOTICE` as its detail. No client renders that text today: the TUI's session card shows only the Error badge, and the desktop shows the `error` status and a generic error entry. The readable record is the `warn!` logged beside it, which exists only when the daemon was started with `DOT_AGENT_DECK_LOG` set (see [Enabling Debug Logs](../troubleshooting.md#enabling-debug-logs)). `DRAFT_CAP_NOTICE` must contain the word `draft`: `scheduler/dispatch/023` keys on it.

## Delegate pointers, the dispatch lock and `pane restart`

A delegate's task pointer is written by `dispatch_one_owned`, which takes two per-pane locks: the **order lock** (`pane_dispatch_order_lock`), held for the whole dispatch, and the **dispatch lock** (`pane_dispatch_lock`), which it shares with `pane restart`. The draft wait runs with the dispatch lock set down (`PaneDispatchHold`): the write parks it before each draft sleep and picks it up again before it takes the pane's writer, so a `pane restart` of the worker proceeds at once instead of waiting out the cap and reporting `NoReply` past its 14-second reply budget (PR #1398 review).

What the dispatch lock protects still holds across the parked stretch:

- **`clear = true` respawn against a restart.** The respawn runs before the pointer write, under the lock; by the time a dispatch parks, it has none left to do.
- **Order of dispatches to one pane.** Kept by the order lock, which only dispatches take and none releases early, so two delegates to one worker are written in the order they queued even while the first waits on a draft. (A second delegate to a worker still owing a `work-done` needs `--supersede` anyway.)
- **Commission accounting.** While parked, the dispatch's commission is counted as in flight again, so the restart's `retire_commissions_of_replaced_agent` leaves it for the dispatch to release itself, and does not undercount a delegate queued behind it.

A restart that lands during the wait replaces the worker, so the write's identity gate refuses the pointer (`WrongSession`, or `NoLiveTarget` while the replacement is still spawning). It is not written to the replacement, where it would otherwise follow nothing the replacement knows about, and the restart has cancelled the task just as it cancels a task already delivered. When the pane has a live occupant by the time the refusal lands, the refusal publishes `DRAFT_WAIT_WORKER_REPLACED_NOTICE` against it, so the card turns `Error` rather than the delegate vanishing; `pane/restart/014` pins this. When it lands while the replacement is not yet live, no notice is published and only the daemon log's `warn!` line for the refusal records it.

## Ordering and the delivery bound

`work-done` hand-offs and dispatch results are delivered from the daemon's hook loop. Awaited inside the hook connection, a draft wait would hold one of the daemon's hook-connection permits for up to the cap, so they are handed off to a per-pane queue instead:

- **One FIFO queue per target pane, drained by at most one task**, which exists only while that queue holds work. Several reports waiting on the same pane's draft are therefore written in the order the daemon received them, each as a message of its own. Across panes deliveries run concurrently, so one pane's draft delays nothing written anywhere else.
- **A panicking delivery ends only itself.** Each delivery runs as its own task; a panic is logged as a warning naming the pane (never its payload) and the drain moves on to the next item.
- **A bound on pending deliveries across all panes**, `MAX_PENDING_PANE_DELIVERIES` (256). At the bound the enqueuing hook connection waits for a slot while holding its connection permit — backpressure onto new hook connections, never a dropped delivery — logged once per saturation episode.
