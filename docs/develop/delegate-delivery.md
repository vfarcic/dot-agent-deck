# Delegate delivery: the echo-gated submit and the in-place re-send

How the deck gets a delegated task pointer into a worker and submitted, and how it recovers when that fails (issues #1383 and #1243). The user-facing summary is [A lost task is re-sent into the same worker](../orchestration.md#a-lost-task-is-re-sent-into-the-same-worker); this page is the mechanism behind it. The code is `src/delegate_retry.rs` (the re-send loop, the delivery id, the composer reading, the ack bookkeeping) and `src/submit_echo.rs` (the first write's Enter), with the wiring in `src/state.rs` and `src/daemon.rs`.

## The two ways a pointer is lost

**Swallowed during boot (#1383).** A delegate's pointer is typed into the worker's PTY once the readiness gate releases. An agent still booting can consume those bytes without acting on them — the terminal-mode switch of a TUI starting up, or an input loop that is not listening yet — and the orchestrator is then left waiting on a healthy worker that never got its task. #1383 measured this on OpenCode, whose readiness gate is a fixed interval because it announces nothing before its first prompt, so no delay can prove it was ready. The readiness buffer in [What `clear` does to delivery](../orchestration.md#what-clear-does-to-delivery) makes the race less likely; it cannot close it.

**Left unsubmitted by the paste window (#1243).** The guarded submit writes the payload, waits `SUBMIT_DELAY` (150 ms, `src/pane_input.rs`), then writes `\r`. An agent TUI that receives the CR together with the text treats the burst as a paste and the CR as a newline inside it, which leaves the pointer in the composer with an empty line under it and no turn. The fixed delay keeps the two apart only while the agent keeps up. Measured against a real interactive Claude Code under 48 busy-loops on 16 CPUs, writing the pointer 1000 ms after `SessionStart` (the respawn path's shape): **3 of 30** pointers were left in the composer, against 0 of 10 on an idle box.

## The first write: an Enter gated on the pointer's echo

Waiting for the agent to *read* the payload is not enough. Polling the PTY slave's unread-input count and writing the CR 150 ms after it reached zero still lost **1 of 30** under the same load: Claude Code had read the text and was still inside its paste window, which runs on a timer that a starved event loop stretches. What does separate them is the agent *painting* the text, which it only does once it has committed the input to the composer. Writing the CR after the pointer's tail appeared on screen submitted **20 of 20** under the same load, with that paint taking up to 1.06 s, and a later run with the final implementation left **0 of 50** unsubmitted.

So `EchoWatch` follows the agent's output from just before the payload is written and reports when the payload's last word — the delivery id — appears on screen one more time than it did before the write. The wait is bounded by `SUBMIT_ECHO_BOUND` (2 s, twice the slowest paint measured); at the bound the CR goes anyway, which is the pre-#1243 behaviour, only later. The `SUBMIT_DELAY` floor still applies, so on a box that keeps up the CR lands when it always did.

- **Opt-in.** A pane that does not echo its input pays the whole bound on every write, with the writer held. The delegate pointer takes the gate because it is the write #1243 lost and a delegation is one write; other automatic writes keep the fixed delay.
- **Eligible payloads** are single-line printable text up to `MAX_ECHO_GATED_PAYLOAD` (512 bytes) whose last word has at least `MIN_TOKEN_CHARS` (6) matchable characters. A multi-line payload is bracketed paste, which agents render as a placeholder, and Claude Code shows a long single line that way too, so neither has anything to match.
- **Bounded work.** The parser is as large as the pane and is rescanned after every batch of output inside the pane's writer. A pane larger than `MAX_ECHO_WATCH_CELLS` (500,000 cells) gets no gate and keeps the fixed delay: PTY axes are accepted up to 4096 each, so without a cap one attach client's geometry could size a 16.7-million-cell parser. 500,000 covers an 8K display filled by a 6 × 12 px font (1280 × 360 = 460,800 cells). The deadline is checked between chunks as well as while waiting for one.
- **A heuristic, not an attestation.** The watch counts the id anywhere on the screen, not in the input box, so output that shows it early — the worker, or a process in its repository, printing the id its task file names — releases the CR early. At worst that is as early as the fixed delay the gate replaced, and the re-send below recovers a pointer left unsubmitted that way.

## The delivery id and the task-file header

Every delegation mints a delivery id, `d-` followed by 8 hex characters (`mint_delivery_id`). It goes in two places:

- at the end of the pointer typed into the pane: `Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]` (`pointer_suffix`);
- in a header the daemon prepends to every worker task file, whatever the role's `prompt_template` says, so no project opts in (`task_file_ack_header`). It asks the worker to run `<absolute path of the deck binary> ack d-7f3a9c21` first, tells it to skip that and carry on if the command fails, is not recognised, or is refused or needs an approval it does not get, and says that a pointer seen more than once is the same task.

The header uses the deck's absolute path for the same reason every generated protocol command does (see [Context handoff](../orchestration.md#context-handoff)), which is why a user's allowlist rule has to name `ack` in that path form next to `work-done`.

## What counts as proof of delivery

After the first write the loop waits for proof that the worker received the pointer. `classify_event` is an exhaustive `match`, so a new `EventType` has to be classified on purpose:

| event from the worker (matched on pane and agent id) | verdict |
|---|---|
| `Thinking`, `ToolStart`, `ToolEnd`, `SubagentStart`, `SubagentStop`, `Compacting`, `PermissionRequest` | received — stop (the daemon's existing "a turn began" predicate, `worker_event_proves_delivery`) |
| `QuotaBlocked` | stop; issue #714's blocked-worker notice owns this case, and typing into a blocked agent only queues more prompts |
| a genuine `SessionStart` | postpone the next attempt by a readiness interval, so it lands after the boot |
| the wrapper's own `SessionStart` (it names the wrapper's session, not the agent's) | ignored |
| `SessionEnd`, `Idle`, `Error`, `WaitingForInput`, `ShellBusy`, `ShellIdle`, `Unknown` | ignored |

**Why not "any event means received":** that would disarm the loop in exactly the case it exists for. Claude Code posts a `SessionStart` early in boot, and OpenCode emits a startup `session.idle` that maps to `Idle`; neither says anything about the pointer.

The loop also ends on an `ack` for the current id, a `work-done` from the pane, a newer delegation (supersede), the pane closing, the agent exiting, the event bus closing, or a lagged event stream — lag may have dropped proof, and a duplicate is the dangerous direction, so it stops as though proof arrived (`RetryEnd`).

## The `ack` command

`dot-agent-deck ack <id>` sends an `AckSignal` over the hook socket, carrying the pane capability token like every other `DaemonMessage` verb ([hook provenance](hook-provenance.md)). The daemon answers at the gate with a `SignalAck`, and `handle_delivery_ack` resolves one of four outcomes (`AckOutcome`):

| outcome | meaning | CLI output |
|---|---|---|
| `Stopped` | the pane's pending delivery; the retry stops and the silent-worker watch armed for the same delivery is cancelled | "Acknowledged" |
| `AlreadyAcknowledged` | the pane's last acknowledged delivery, acknowledged again; a no-op | "Acknowledged" |
| `NotPending` | the pane's current delivery with no retry pending — the loop already ended (usually because a turn began, which a real agent reports before the `ack` it runs as a tool) or was never armed (retry off, agent type not retried, a Pi seed delivery) | "Acknowledged" |
| `Unknown` | not the pane's current delivery — mistyped, an earlier delegation's, or one presented by another agent on the pane while its retry is pending; the retry keeps running. A malformed id is refused by the CLI before it sends anything, and by the daemon if one arrives anyway (logged only as a length) | "no delivery under that id for this pane … it may send the task pointer again", on stderr |

The CLI **exits 0 in every case** (`ack_report` in `src/main.rs`). Anything that is not a matched reply — a malformed id, no managed pane, an unreachable deck, no answer, a refusal, a reply this build does not understand — prints "Could not confirm receipt with the deck (…); this is harmless — carry on with your task." on stderr. A worker told to acknowledge first must never be derailed from its task by a failed acknowledgement. An ack is a claim that the worker read its task file, not a receipt for the work.

**Why the ack cancels the went-quiet report.** An ack is not an agent event, so the silent-worker watch cannot see it. A worker whose agent emits no events of its own would otherwise be reported for staying quiet right after acknowledging, so a `Stopped` ack cancels that watch explicitly, the same way `work-done` does.

**Cross-version.** The hook socket is not versioned by `PROTOCOL_VERSION`; [versioning.md](versioning.md) has how `ack` degrades in both pairings. In short: an older daemon logs the unknown message as malformed and answers nothing, so the CLI reports it could not confirm receipt and exits 0; an older binary in the worker pane rejects `ack` as an unknown subcommand, the task file tells the worker to carry on, and the re-send stops on the worker's own turn events.

## One re-send: probe, read, then maybe retype

Every re-send starts with a **submit-only probe** — a bare Enter — and then reads the worker's screen (`classify_composer`), replaying the pane's scrollback through the same terminal parser the TUI renders with:

- **In the composer.** The pointer ends right at the cursor, at the end of what was typed — where Claude Code, Codex and OpenCode leave the cursor. It is most likely sitting there unsubmitted (#1243's shape). The re-send never types it again; after the grace below it presses Enter once more if the pointer is still in the composer, because an agent that took the first write's CR into a paste ignores the next Enter and submits on the one after. A pointer that was in the composer when a re-send started is not retyped by that re-send even if it has left the screen by the end of the grace. Devin and Pi were not measured; one that leaves its cursor away from typed text gets the first Enter on every re-send but not the second.
- **In the transcript only.** The id shows above the input box, so the pointer was most likely submitted. That re-send is the probe's Enter alone, with no second Enter that could submit whatever else the composer holds by then. The observation is **sticky**: once any reading has seen it, the pointer is never typed again for this delivery, even if the worker's output later scrolls the id off screen. Later re-sends still press their Enter, because a worker that is not yet reading its terminal shows the task and each Enter the same way.
- **Unreadable.** The pane was resized since the pointer went in and the worker has not redrawn (a resize blanks what the deck can read), the screen is empty or cannot be parsed, or the pane is larger than 500,000 cells. The Enter is the whole re-send.
- **Absent.** The screen cannot tell an empty composer from one holding the pointer where the screen does not show it, so the loop waits a **grace** — `probe_grace`, 5 s or half the wait before the next re-send if that is shorter — for the turn that Enter would start, and types the pointer again, with the same id, only if the worker is still silent.

The loop skips a re-send entirely if someone has typed into the pane since the deck last wrote to it, so it does not type onto a draft started after the task went in. It holds no dispatch lock between attempts, so a superseding delegate does not queue behind it.

## The schedule

`DOT_AGENT_DECK_DELEGATE_RETRY_SCHEDULE_MS` is a comma-separated list of waits in milliseconds, read at arm time and never cached (`RetrySchedule::parse`). Entry *i* is the wait from the first write (for *i* = 0) or from the start of re-send *i* to the start of re-send *i + 1*; a probe's grace and its retype come out of that wait. After the last re-send the loop waits the last entry once more before declaring the delivery exhausted, so N entries give N re-sends and a nominal span of `sum + last`.

- Default `20000,40000,80000` (`DEFAULT_RETRY_SCHEDULE_MS`): re-sends at 20 s, 60 s and 140 s, exhausted at 220 s.
- Each entry is clamped to `MIN_RETRY_WAIT`..`MAX_RETRY_WAIT` (100 ms to 5 minutes); at most `MAX_RETRY_ENTRIES` (8) are read, so a pasted list cannot turn one delegation into an unbounded stream; a list that does not parse falls back to the default; `0`, empty or whitespace turns the re-send off and restores the single write.
- A genuine `SessionStart` after the write, or a busy dispatch lock, can stretch any wait. The went-quiet report therefore waits for the loop's real end rather than for the nominal span, is suppressed if the loop ended on proof or on a lagged or closed event stream (`silence_retry_end_proves_delivery`), and otherwise quotes how many re-sends there were ("The deck re-sent the task pointer into the same process N times and none of them produced an event."). With that report switched off (`DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS=0`), the loop logs exhaustion itself.

## Which workers are retried, and which are retyped

`agent_type_supports_retry` covers Claude Code, OpenCode, Pi, Codex and Devin. A pane whose agent the deck cannot identify — a launcher such as `devbox run …` with no `agent` line — gets **no retry**: it has no channel that could ever report a turn, so every attempt would be a potential duplicate. Declaring the agent covers it (CLAUDE.md rule 20). A Pi role with `clear = true` pulls its task through its extension rather than having it typed, so there is nothing to retry. A known agent whose hooks or plugin are not installed emits nothing either; it still gets the bounded re-sends, and while the pointer is visible each is an Enter rather than a copy, but where it is not visible such a worker can never answer the Enter with a turn the deck hears, so each re-send ends with the pointer typed again.

`RetypePolicy::for_worker` decides once, at arm time, whether a re-send may type the pointer again. It returns `Never` for a pane the deck spawned as a wrapper host or whose agent's integration strategy is `Wrapper` — today, Codex. When Codex's prompt hook is untrusted or switched off (#1390's wrap-only Codex), the wrapper is the only thing reporting on that pane and it cannot report a submitted prompt: such a Codex can take the pointer, clear it from its screen and start working without the deck hearing a turn, so a copy could start the task twice. The policy is keyed on the launch shape rather than on the wrapper's per-event `wrapper_prompt_reports_unavailable` marker, because a freshly respawned wrapper may not have emitted a single event by the first write, so the marker's absence proves nothing yet. That withdraws the retype from a Codex whose prompt hook works too, which costs it a recovery the Enter does not give, never a duplicate. The wrapper's `Thinking` is no evidence either way: it is classified from painted output, not reported by a prompt hook. Under `Never` a re-send still presses the probe's Enter and the second Enter over a pointer left in the composer.

## Trust bounds

Both signals the loop trusts are same-user and unattested, and [hook-provenance.md](hook-provenance.md) records them beside the others:

- **Forged turn events.** Raw `AgentEvent`s carry no provenance token for any agent today, so a same-user process that knows a worker's pane and agent ids can forge a `Thinking` and stop that worker's re-sends. Requiring attestation would stop the retry for every real hook too. A forged event costs what the deck did before #1383 — the pointer is not re-sent — and the went-quiet report still covers a worker that then says nothing.
- **Early echo.** Covered above: printing the id early makes the first Enter as early as the fixed delay it replaced, and reaches only that worker's own submit.

## Residuals

- **A second copy is unlikely, not impossible.** The retype decision reads the screen, not the composer: a composer that neither shows its text nor submits it on Enter, or an agent that starts a turn without reporting one within the grace, still gets a second copy. The task-file header's "same task" line is the backstop.
- **The probe's Enter submits whatever the composer holds.** A worker whose input box holds unsent text of its own when a re-send fires would have it submitted. A re-send only fires after the worker has said nothing at all since the pointer went in, when the box should hold the pointer or nothing, and not after a human has typed into the pane.
- **The echo gate is a heuristic** (above).
- **A late event from a superseded delegation can stop a newer one's re-sends.** Hook events carry no delivery id, so on a `clear = false` worker a turn event still arriving from the earlier delegation is taken as proof for the newer pointer, which then stays typed once with no re-send — the safe direction, never a duplicate, and the same trade a stale `work-done` makes.

## Logs

Every re-send and every end of the loop logs a `delegate retry:` line at `info` or `warn` with the pane, the role, the delivery id and the attempt, saying whether it retyped the pointer, pressed Enter, skipped, or stopped and why. Grep the daemon log for the id at the end of the pointer line. Each `ack` logs its outcome at `info` from `handle_delivery_ack`.

## Tests

- `src/delegate_retry.rs` and `src/submit_echo.rs` unit tests: schedule parsing, event classification, composer classification, the retype policy, the ack bookkeeping and the loop against a fixture registry.
- `tests/e2e_delegate_retry_in_place.rs` (lane 1): catalog `orchestration/delegate/041`–`044`, `046`, `047` — a pointer swallowed during boot retried into the same process, exhausted retries reported without a respawn, submit-only retries over a visible composer, a hookless worker's `ack` retiring the retry and its silence watch, and Claude- and Codex-shaped composers that drop their first Enter.
- `tests/e2e_delegate_respawn_readiness.rs` (lane 2): `orchestration/delegate/045`, a real interactive OpenCode worker completing a delegation whose first pointer is sent during boot.
