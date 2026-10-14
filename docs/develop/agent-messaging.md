# Agent messaging

Maintainer reference for the per-agent inbox and wake integrations. User behavior and setup belong in [Orchestration](../orchestration.md#delivery-in-the-tui-and-desktop-app). The core is `src/inbox.rs`; `src/inbox_hub.rs` owns connections, rendering and notifications; `src/inbox_cli.rs`, `src/inbox_prompt.rs` and `src/doorbell.rs` connect it to agents. Both clients consume the daemon's inbox summaries and broadcasts.

## Inbox model

The daemon owns an in-memory inbox. Restarting it loses inbox contents. Messages have ids of the form `m-<eight-character-deck-id>-<16-hex-random-bits>`, generated with OS randomness; an id is also the dedup key. A persistent deck id distinguishes decks, while the random suffix separates messages across restarts. Direct addresses are `inbox.<deck>.<encoded-pane>`; the recipient binding also names the current agent generation. An exited generation's bound messages become undeliverable rather than moving to its replacement. Unbound messages can wait for a pane's next agent.

| State | Meaning and next transition |
|---|---|
| `queued` | Eligible for one consumer to fetch. |
| `fetched` | Held by that consumer, awaiting acknowledgement. Ack settles it; nack, stream loss or ack-wait makes it available again. |
| `acknowledged` | Receipt recorded. This is not task completion. |
| `claimed` | An offer has a time-limited owner. Completion settles it; release, expiry or owner exit returns it to the pool. |
| `undeliverable` | Delivery limit reached, recipient exited, inbox closed or message superseded. The reason determines whether a card notice is owed. |

The default ack-wait is 60 seconds and `max_deliver` is 5. Retry preserves the id; consumers should act on an id once. Dedup remembers up to 4096 ids for ten minutes. The pending limits are 256 per recipient and 4096 daemon-wide, with a 1 MiB body limit; a bound refuses explicitly rather than evicting pending work. Terminal history retains at most 64 messages per pane, 1024 daemon-wide and 16 MiB of bodies, evicting the oldest terminal messages first. Terminal bodies expire after one hour while summaries remain within the count bounds; closing a pane drops its terminal history. Pending work is not trimmed to make history space. Stored subjects are one line of at most 200 characters.

Publish fans out copies with one id, with state per recipient. Subscription resolution excludes the sender and stays inside the sender's orchestration instance or dispatch tree. Offers go to one eligible subscriber at a time; acknowledging an offer claims it. The default claim is 30 minutes, with a 60-second minimum and 24-hour maximum. Release, expiry and owner exit exclude the former holder from subsequent assignment. Publish and offer share a per-agent budget of 60 per minute; deck-generated messages are exempt.

### Delegated-task retirement

A newer delegation or a pane restart retires the worker's earlier unacknowledged tasks as superseded, and superseded tasks are not reported. A worker's `work-done` names no task, so the deck infers which one it answers. If the worker acknowledged a task since its previous `work-done`, the completion answers that task, and only tasks that have reached the worker since (fetched at least once, not acknowledged) are retired with it. A newer task it never fetched stays queued and watched, so the orchestrator is still told if it does not reach the worker. Otherwise the worker answered a task it took another way, such as its task file, and every unacknowledged task to its pane is retired. A late `work-done` therefore cannot cancel a newer queued task, except in this case: an earlier task was acknowledged and its `work-done` never arrived, and the newer task was then taken from its file without being acknowledged.

### Publish and offer integration

`src/inbox_pubsub.rs` resolves live pane membership from the registry and the daemon's orchestration, role, working-directory and dispatch-return maps. It reads each member directory's project configuration. `subscribe` on an orchestration or role supplies topics; role `take_offers` supplies job pools; `[dispatch] subscribe` subscribes the caller to its dispatch tree. Omitted keys inherit through `extends`; an explicit list, including an empty one, replaces the inherited list. Names match `[a-z0-9][a-z0-9-]{0,63}`, with at most 64 entries per list.

The CLI exposes `inbox publish <topic> --text|--file`, `inbox offer <pool> --text|--file [--claim-ttl <duration>]`, `inbox complete <id>` and `inbox release <id>`. A body uses exactly one input option; `--file -` reads stdin, bounded at 1 MiB. `--claim-ttl` accepts unit-bearing durations such as `30m`, `1h30m`, `90s` or `500ms`; the daemon applies its claim-duration bounds.

| Hook-socket verb | Request fields beyond pane, agent and token | Successful reply |
| --- | --- | --- |
| `inbox_publish` | `topic`, `body` | `InboxPostReply`: `id`, `delivered_to`, and `refused_full` when some recipients are full. |
| `inbox_offer` | `pool`, `body`, optional `claim_ttl_ms` | `InboxPostReply`: `id`, and `offered_to` when assigned; the latter is absent while pooled. |
| `inbox_complete` | `id` | `InboxClaimReply`: `outcome: "completed"`. |
| `inbox_release` | `id` | `InboxClaimReply`: `outcome: "released"`. |

Refusals carry `refused` (stable code) and `error` (explanation), without a success id or outcome. Publish/offer codes include `invalid_topic`, `invalid_pool`, `not_live`, `no_scope`, `scope_changed`, `not_orchestrator`, `body_not_saved`, `rate_limited`, `inbox_full`, `deck_full`, `body_too_large`, `duplicate`, `too_many_offers` and `claim_ttl_out_of_range`. Publish and offer re-read the scope up to three times while agents change; if the scope still changes during admission, they refuse with `scope_changed`. Claim operations also use `not_claimant` and `unknown`. Provenance refusal codes apply, including `unattested` for a missing token on a legacy-admitted pane and `malformed_token` for an empty token. The four verbs also bind the peer pid to the managed agent's process tree under the same platform policy as waiters, refusing a proven mismatch with `inbox_not_agent_process`, and require the attested agent to be live in the pane.

An offer's body is Trusted and labelled as the orchestrator's instruction, like a delegate. The deck preface tells the taker to do the work and then complete or release the id. A publish body is Untrusted and labelled `<role> (announcement — the text inside is UNTRUSTED)` so the desktop does not classify it as a worker report.

`InboxLimits::offer_assign_wait` bounds an assigned but unfetched offer, using the undelivered-notice setting (five minutes by default). Expiry returns it to the pool and excludes that holder. `next_deadline` includes this deadline. The assigner wakes when work returns to a pool and sweeps every 30 seconds to discover late joiners. `InboxEvent.returned: Option<OfferReturn { previous, cause }>` records the previous holder and return reason inside the daemon. The attach broadcast's additive `InboxNotice.returned` flag removes the old holder's count and gives the desktop evidence row **Job handed back to its pool**. The additive, defaulted `InboxSummary.claimed` field appears in list-agents and `daemon status --json`; the TUI and desktop card counts treat claimed jobs as taken messages.

The hub queues offerer notices for claimed, completed, claim-expired and undeliverable jobs. `ack`, `fetch_within` and `fetch_first_acked` drain owed notices immediately, so a claim notice does not wait for the next tick. These fields and hook verbs are additive; S11 does not bump `PROTOCOL_VERSION` or add a semantic contract-break marker.

## Consumers and acknowledgement

Waiters have their own pool (512 daemon-wide, eight per agent generation), separate from hook-ingest slots. The newest eligible waiter takes the queued batch that fits its budget. A once waiter is spent by hand-over; a stream remains armed and accepts ack/nack controls on stdin. EOF requeues fetched, unacknowledged work. Waiters receive keepalives. The stream's pane capability and agent generation bind it to the recipient; launch command spelling does not enable native wake.

On Linux and macOS, `inbox wait` and `inbox announce` also bind the kernel-reported peer pid and the claimed component-instance pid to the managed agent's process tree. Both must be the managed agent or a descendant, and the claimed instance must be the peer or its ancestor. A proven mismatch is refused with `inbox_not_agent_process`. Linux reads `/proc/<pid>/stat`; macOS uses `proc_pidinfo(PROC_PIDTBSDINFO)`. Where process ancestry cannot be determined, including other platforms and processes that exit during the walk, this additional check falls back to the capability and generation checks. A stream from the same component instance supersedes its predecessor and requeues unacknowledged work; a different live instance is refused. A generation's native receiver is pinned to the first instance that arms or announces, resolving through hook shells to the agent rather than pinning a transient hook shell. A different instance cannot replace it after a stream closes, and announce can be refused with `another_component_armed`. An unnamed instance does not create a pin and cannot replace an existing one.

The inbox verbs bind authority to the agent's process tree: a process the agent runs, such as a command in its tool shell or a nested agent of another kind naming the outer agent's process as its component instance, acts with the outer agent's authority and can act as its receiver. The pin keeps a second non-shell instance out once one has armed or announced; it does not distinguish the agent from processes it runs that use its instance identity. For Claude Code below the 2.1.296 waiter gate, the deck's generated settings omit the waiter and its SessionStart hook does not announce native wake, so the first arriving receiver instance pins the generation.

| Agent | Component | Receipt signal |
|---|---|---|
| Claude Code | `SessionStart` and `Stop` asyncRewake once waiters (`inbox wait --agent claude`) | The waiter writes framed blocks to stderr and exits 2. A subsequent attested deck-message prompt observes the ids. Doorbell `UserPromptSubmit` fetches and acknowledges when injecting context. |
| Codex | `UserPromptSubmit` doorbell injection and `Stop` turn-end hand-over | The hook fetches and acknowledges with an inclusive 6000-byte budget. Stop returns `decision: block` with the framed content. Without trusted hooks, model-run `inbox fetch` acknowledges receipt. |
| OpenCode full TUI | Separate `dot-agent-deck-tui.js` TUI plugin; stream component `opencode_plugin` | `promptAsync`, then an assistant `message.updated` acknowledges pending deck messages in that session at or before its `parentID`. This covers coalesced turns; resolving `promptAsync` alone is insufficient. |
| OpenCode `--mini` | Server plugin stream component `opencode_mini` | An assistant turn acknowledges its parent or any earlier user message in the same session; ack and session-idle request a repaint nudge. |
| Pi | Extension stream component `pi_extension`, started per `session_start` | `sendUserMessage(text, { deliverAs: "followUp" })`, then a committed user `message_start` carrying the opener. Never acknowledge on the pre-commit `input` event; `message_end` tracks the turn reply, not receipt. The `input` hook strips the attempt line and consumes replaced or committed hand-overs; retries follow [Pi extension](pi-extension.md#native-inbox-delivery). |
| Devin | Doorbell-only integration | Model-run `inbox fetch` or explicit `ack`. No native wake is claimed. |

OpenCode targets the session on screen, creates and navigates to one from the home route, and returns a child view to its root. It supplies model and agent from the target's last user message, then process history, then argv; a UI-only model selection before any prompt is not exposed to this path. The server plugin intercepts a doorbell in `chat.message` only in the full TUI's worker thread or `--mini`, fetches with `ack: false`, and replaces the text in place; an assistant turn acknowledges pending deck prompts in the same session through its parent; human messages and prompts in another session are not acknowledged. Main-thread `run`, `serve` and `acp` do not intercept it. Reassigning `output.parts` or transforming historical messages is not equivalent. `attach`, `acp`, `web`, `pr` and `github` remain unmeasured; the user docs claim doorbell fallback there.

The mini repaint nudge shrinks the PTY by one column, waits 300 ms by default, then restores it. It bypasses recorded geometry and scrollback changes. A concurrent real resize wins. Only `opencode_mini` streams request this nudge.

Pi kills its watcher on `session_shutdown` and starts a new one on session start, including `/new`, fork, switch and reload. Late callbacks are guarded by extension generation and caught so a stale context cannot kill Pi. Stream exit 3 means outside a deck or missing capability: Pi stops retrying rather than looping forever. Other watcher failures use bounded backoff. Its doorbell `input` handler fetches without acknowledgement, marks the original line handled and submits the framed content; receipt still waits for the committed user message.

Pi tags each native hand-over attempt and removes the tag in its `input` handler, consuming superseded attempts and messages already committed. An idle recheck retries an attempt the handler has not seen; after `input`, a retry needs the refusal witness described in [Pi extension](pi-extension.md#native-inbox-delivery). Without that witness the message waits for inbox ack-wait redelivery, which replaces the attempt when Pi has nothing queued. Unanswerable status queries do not trigger a retry. This guard cannot recall an attempt already past `input`; receipt and retry retain the inbox's at-least-once semantics.

### Long turns: hold and the late ack

A stream component may answer a redelivery with `{"hold":[id]}` when it still holds that message, queued in the agent behind the turn in progress, and did not hand it over again. The deck refunds that redelivery's hand-over, and the count never drops below one. The refund applies while the message is fetched by that stream, is not an offer, and was first fetched less than one hour ago (`DEFAULT_HOLD_BOUND` in `src/inbox.rs`). Past that bound, `max_deliver` applies as it does to every consumer. A refunded hold also moves a delegated task's undelivered-notice deadline a full `DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS` past the hold. A daemon that predates `hold` reads the line as an empty ack.

Pi answers `hold` for a redelivered id that it is holding behind an admission, or that a hand-over still carries and that may still commit. A redelivery that replaces a hand-over lost after `input` is not a hold: the replaced hand-over did not reach the agent.

The OpenCode TUI and `--mini` plugins remember the session each deck prompt went to, and they follow `session.status` and `session.idle`. A redelivery of a prompt that is still pending, in a session that has stayed busy since, is answered with `hold` and is not prompted again. If the session goes idle with the prompt unanswered (an abort), or `message.removed` removes its user message (a revert), the next redelivery prompts it again. With no busy status seen for the session, a redelivery prompts again, as before.

An ack from the bound agent for a message that became undeliverable after `max_deliver` hand-overs is recorded: the message becomes acknowledged and the ack answers `acknowledged`. An offer is not changed, and a message undeliverable for any other reason still answers `undeliverable`. The daemon remembers up to 256 such messages for one hour (`LATE_ACK_MAX_RECORDS` and `LATE_ACK_MEMORY` in `src/inbox_hub.rs`).

When a delegated task is acknowledged late, the orchestrator generation that was told it was not delivered gets a correction with the subject `task delivered after all`. The worker then counts as having received a task for work-done retirement.

The recipient's card is lifted from its not-delivered Error to Thinking by a daemon-authored event carrying the `delivery_notice_cleared` metadata key. This happens once no other message the card was told about is still owed a correction, and only while the daemon's copy of the card still shows the deck's notice. Both the TUI and the desktop app consume this event.

Claude Code's once waiter has no channel to answer `hold`, so a message waiting behind a long Claude turn can still be redelivered and reported not delivered before it arrives. The orchestrator is then told it arrived.

## Claude version gate and framing

`CLAUDE_ASYNC_REWAKE_MIN` in `src/hooks_manage.rs` is 2.1.296, the measured release, not a claim that older Claude versions lack asyncRewake. Unknown or lower versions get no waiter. SessionStart announces native wake only when the installed Claude settings contain the deck's SessionStart waiter; without it, including an installation below the version gate, no native announcement delays the doorbell. The waiter timeout is 86,400 seconds; `/clear` may leave several waiters, so inbox hand-over is claim-once. Once waiters poll their agent ancestry for orphan exit; stream watchers exit when their owning integration closes stdin.

The waiter settings include `rewakeMessage` and `rewakeSummary`. Both keys are `@internal` in Claude Code's schema, observed in 2.1.294–2.1.296, and are written only under the same 2.1.296 gate as asyncRewake. `rewakeMessage` explains that this is a deck message rather than a hook error; `rewakeSummary` is `✉ dot-agent-deck message`. Reinstall consolidates existing waiter entries and refreshes these values. If upstream removes the keys, default hook wording may return: retain session framing that agrees with Claude's automated-notification label.

`SESSION_FRAMING` is delivered through Claude's `SessionStart` context and doorbell injection; `AGENT_FRAMING` is the agent-neutral counterpart used by CLI fetch and Codex. They identify the deck as the workspace the user started, explain that work arrives as automated messages rather than fresh user approval, tell the agent to act on an id once, and distinguish UNTRUSTED reports from instructions. Do not add fixture-only authorization to live tests: the production framing must suffice.

Each block starts `[dot-agent-deck message <id> from <sender>]`, then `Sent by: <provenance>.`, and ends `[end of dot-agent-deck message <id>]`. Provenance comes from the attested sender or a deck-generated origin, not the worker's text. Agent-authored reports carry an inner UNTRUSTED fence. Forged framing tokens in content must be defused after normalizing text. Budget truncation must retain the enclosing fences and a safely rendered path to the full body when one is provided.

Before posting a message whose body cannot fit the tightest hand-over budget, the daemon saves its full content to an exclusive, owner-only file in the recipient's working directory under `.dot-agent-deck/`, falling back to the deck state directory. Untrusted content keeps its report fence and has forged message markers neutralised. The delivered excerpt names the saved file using an inert location description. If neither location can save it, the post is refused and nothing is posted; an acknowledged excerpt must not silently discard the rest of an unsaved body.


## Retired delivery interfaces

S12 removes `delegate_retry`, `submit_echo`, `prompt_delivery`, `WriteAndSubmit`, `SubmitText`, guarded task-body sends, provisional prompt confirmation, readiness buffers, the seed fallback and the draft cap. `get-seed` remains an inbox compatibility alias; `--reports-prompts` is a hidden no-op. `ack d-...` returns `unknown`: only inbox message ids can be acknowledged, and the former `stopped`/`not_pending` delivery outcomes are gone. Human input from both clients uses `WriteInput`.

The daemon warns at startup for each set entry in `REMOVED_DELIVERY_SETTINGS` without logging its value. The removed reuse debounce and desktop seed buffer have no effect either.

## Doorbell guards

The only automatic text typed into an agent's composer is the fixed doorbell: ` dot-agent-deck: message <id> waiting, run: dot-agent-deck inbox fetch`. It contains no task body. Eligibility requires queued work, no armed consumer, an elapsed native-arm grace, a hook-reported idle moment, and no unsent human draft. Unknown status, busy agents, permission prompts and unknown dialogs hold it. The guard applies to input from either client; no draft-deferral cap overrides it.

Codex additionally requires a recognised empty composer on screen, so an Enter interpreted as a newline does not cause the next ring to submit the user's draft. If Codex changes its UI and the recogniser stops matching, idle ringing waits; a user-started turn still enables the Stop hand-over. Native arm grace prevents startup ringing before a component has had time to connect. Doorbell retries have their own schedule and do not increment inbox hand-over attempts.

Eligibility is revalidated under the PTY writer before each automatic write step. A per-pane admission epoch changes when the agent, doorbell-relevant status, writability or wrapper fork changes, invalidating a previously scheduled ring; an armed consumer also cancels a ring.

## Environment reference

These are developer and test tunings, not ordinary setup requirements. Daemon-read settings need a newly started daemon; changing a client's environment does not affect an existing daemon. All `INBOX_*` limit overrides below require positive integers and otherwise retain defaults. Count conversions saturate at their target integer size. Millisecond units are explicit in the names.

| Variable | Default | Reader and meaning |
|---|---|---|
| `DOT_AGENT_DECK_INBOX_ACK_WAIT_MS` | `60000` | Daemon: fetched-message ack deadline. |
| `DOT_AGENT_DECK_INBOX_MAX_DELIVER` | `5` | Daemon: maximum unacknowledged hand-overs. |
| `DOT_AGENT_DECK_INBOX_MAX_PER_RECIPIENT` | `256` | Daemon: pending work per recipient. |
| `DOT_AGENT_DECK_INBOX_MAX_MESSAGES` | `4096` | Daemon: live messages overall. |
| `DOT_AGENT_DECK_INBOX_MAX_BODY_BYTES` | `1048576` | Daemon: body size limit. |
| `DOT_AGENT_DECK_INBOX_HISTORY_PER_PANE` | `64` | Daemon: terminal history per pane. |
| `DOT_AGENT_DECK_INBOX_DEDUP_WINDOW_MS` | `600000` | Daemon: dedup retention time. |
| `DOT_AGENT_DECK_INBOX_DEDUP_MAX_IDS` | `4096` | Daemon: dedup id count. |
| `DOT_AGENT_DECK_INBOX_MAX_OFFERS_PER_SCOPE` | `256` | Daemon: open offers in a scope. |
| `DOT_AGENT_DECK_INBOX_RATE_PER_MINUTE` | `60` | Daemon: shared agent publish/offer budget. |
| `DOT_AGENT_DECK_INBOX_MIN_CLAIM_TTL_MS` | `60000` | Daemon: minimum claim duration; raises default and maximum if necessary. |
| `DOT_AGENT_DECK_DOORBELL_TICK_MS` | `200` | Daemon: eligibility scan period; positive integer. |
| `DOT_AGENT_DECK_DOORBELL_SCHEDULE_MS` | `15000,30000,60000,120000` | Daemon: positive comma-separated waits between rings; last wait repeats. Invalid or empty schedules retain the default. |
| `DOT_AGENT_DECK_INBOX_ARM_GRACE_MS` | `10000` | Daemon: wait after native announcement before fallback; zero is allowed. |
| `DOT_AGENT_DECK_INBOX_UNDELIVERED_NOTICE_AFTER_MS` | `300000` | Daemon: delegated message still unacknowledged produces an orchestrator notice; also bounds an assigned, unfetched offer before returning it to its pool. Zero/invalid retains default. |
| `DOT_AGENT_DECK_INBOX_KEEPALIVE_MS` | `30000` | Daemon: idle waiter keepalive; zero/invalid retains default. |
| `DOT_AGENT_DECK_REPAINT_NUDGE_GAP_MS` | `300` | PTY registry: mini repaint interval; parsed as `u64`, invalid retains default. |
| `DOT_AGENT_DECK_INBOX_WATCHER_BACKOFF_MS` | doubling `1000` → `30000` | Pi extension and both OpenCode watchers: fixed respawn delay override, integer `1`–`3600000`; invalid retains normal backoff. |
| `DOT_AGENT_DECK_INBOX_TEST_PAUSE_WAITER_DELIVERY` | unset | Test-only daemon switch: truthy holds delivery while keeping waiters armed, alive and deregisterable. |
| `DOT_AGENT_DECK_INBOX_TEST_UNBOUND_COMPONENTS` | unset | Compiled with the `e2e` feature only: `1`, `true`, `yes` or `on` bypass the additional process-tree binding for synthetic wire tests. Builds without `e2e` ignore it. Real-agent tests must leave it unset. |
| `DOT_AGENT_DECK_HOOK_IDLE_TIMEOUT_MS` | `60000` | Test-only daemon setting for ordinary hook-connection idle timeout. Inbox waiters outlive it; zero/invalid retains default. |

The process also uses the deck-provided `DOT_AGENT_DECK_PANE_ID`, `DOT_AGENT_DECK_AGENT_ID`, `DOT_AGENT_DECK_PANE_CAPABILITY` and `DOT_AGENT_DECK_SOCKET` to address and authenticate inbox commands. They are identity inputs, not timing knobs. See [Hook provenance](hook-provenance.md).

## Contract and validation

This change moves `PROTOCOL_VERSION` from 10 to 11. The semantic compatibility marker and `changelog.d/1666.breaking.md` must remain paired under rule 12. `SessionSnapshot.delivery_notice` and inbox status summaries are additive; older daemon hook behavior can omit the concise deck-message prompt on the card because the newer hook clears raw message text. Both clients must retain the not-delivered notice through attachment and resync.

Covering tests include `inbox/core`, `inbox/cli`, `inbox/status`, `inbox/doorbell`, `inbox/claude`, `inbox/codex`, `inbox/opencode` and `inbox/pi` in `tests/CATALOG.md`. Lane 2 runs locally with real agents; named live runs, load-test evidence and the final CI lane-1 result belong in the PR report. Desktop word parity is pinned with `tests/fixtures/inbox-display.json`, shared by Rust and TypeScript tests.
