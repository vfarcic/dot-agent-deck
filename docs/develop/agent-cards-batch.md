# Batch card actions (issue #2399)

Issue #2399 is step 2 of PRD #2374's agent cards: one card action sent to many agents in one press, from a card with agents under it or from a selection in the Cards view. The user page is [`docs/desktop/agent-cards.md`](../desktop/agent-cards.md) ("Send one action to many agents"); this page holds the wire, the checks and the decisions a maintainer needs. The tree the batch follows is [`agent-cards-tree.md`](agent-cards-tree.md).

Per CLAUDE.md rule 18 the **daemon** decides who is included, what text each card gets and whether each delivery may go; the desktop sends the entries the user picked and draws the daemon's answers. The TUI has no cards, so it has no batch (PRD #2374's documented gap); the CLI's `card act` stays one card at a time.

## The wire: one verb, one capability

`AttachRequest::InvokeCardActionBatch` (`op: "invoke-card-action-batch"`, `src/daemon_protocol.rs`):

| field | what it holds |
| --- | --- |
| `mode` | `"preview"` or `"execute"` (`CardBatchMode`) |
| `action_id` | the action, by the id the cards list it under |
| `request_id` | names the batch: 1 to 124 bytes of `[A-Za-z0-9._:-]` |
| `entries` | `CardBatchEntry`: `pane_id`, `agent_id`, and for `execute` the `action_token` and `instance_id` the preview answered |

The answer rides on `AttachResponse.card_action_batch` (`CardBatchResult`: `request_id`, `entries`), one `CardBatchOutcome` per distinct entry, in the request's order. `outcome` is `included` / `skipped` for a preview and `delivered` / `closed` / `failed` for an execution. An included entry carries `name`, `label`, `kind`, the exact `prompt` with its `prompt_source` (absent for a close), and the `action_token` and `instance_id` to execute with. A skip or failure carries a stable `code` and a `message`. An entry that fails leaves the response's `ok` true; `ok: false` is reserved for a request refused as a whole (`check_batch`: a malformed request id, an empty or oversized action id, no entries, more than `MAX_BATCH_RAW_ENTRIES` = 512 entries counting repeats, an oversized identifier, more than `MAX_BATCH_ENTRIES` = 256 distinct cards).

**The daemon advertises `agent-cards-batch`** (`CAP_AGENT_CARDS_BATCH`) on every platform. `DaemonClient::invoke_card_action_batch` is the one sender and sends only on a connection whose own Hello named it and `hello-follow-on`; it binds the connection to the `instance_id` the cards were listed by, as `invoke_card_action` does. `AgentCardsAnswer.batch` (set by `tree_gate`) tells the desktop whether to offer batches; the Tauri command is `agent_card_invoke_batch`, and the webview's runtime member is `invokeCardActionBatch(deckId, actionId, { mode, entries, requestId, instanceId })`.

**Rule 12 answer: no `PROTOCOL_VERSION` bump and no `.breaking.md`.** The request is a new verb every sender withholds until the daemon advertises it (rule 18's capability rung), the response field is additive and optional, and nothing about `invoke-card-action` or the card list changed meaning.

## Why preview, then execute

A batch is always confirmed, whatever each action's own `confirm` says (PRD #2374, #2399 scope), and the confirmation must show exactly the text each agent will get. Two calls make that hold:

- **preview** reads a fresh computation of the cards (`refresh`) and classifies each entry with `preview_entry` — the checks a single press makes in `invoke_once`, less the ones a preview cannot make. It sends nothing and remembers nothing. Skip codes are a single press's refusal codes: `unknown_entry` / `stale_entry` (no such card / the pane holds another agent), `unknown_action`, `client_side_action` (Open), `text_required` (a reply: the user's own words, one card at a time), `action_disabled`, and `agent_stopped` (`CODE_AGENT_STOPPED`, the `REASON_AGENT_STOPPED` word) for a prompt action whose agent is not running.
- **execute** takes only what the user confirmed, each entry with the `action_token` and `instance_id` its preview answered, and runs each through `invoke` — the single press's own path: the request-id claim and admission caps, the instance check (`daemon_replaced`), the token check (`stale_action`), delivery through `post_card_message`, and the record on that card's `recent_deliveries`. So an entry whose action changed after the preview fails on its own with `stale_action` and is sent neither text, and the entries after it still go (`execute_batch` does not stop on a refusal).

Each entry is invoked under its own request id, `<batch request_id>:<index>` (`batch_entry_request_id`), which is why the batch id is limited to 124 bytes: a repeat of the same execution repeats each entry's single press and, while that press's answer is remembered (up to `REQUEST_ID_MEMORY`, 10 minutes, sooner when newer answers need the room — the single press's retention), is answered from the store, sending nothing again. A repeat after the answer is forgotten is a new press and delivers again. `confirm` is `true` on every entry — the batch's confirmation is the user's confirmation of each — and `text` is always `None`, so no reply goes in a batch. Entries run one after another, not in parallel: each takes the card store's turn anyway, and a close waits for its process.

Duplicates are dropped before anything is acted on (`distinct_batch_entries`: the first naming of a pane and agent wins), so a card is never pressed twice by one batch. `check_batch` bounds a request in three steps, each before the next (audit S2): at most `MAX_BATCH_RAW_ENTRIES` (512, twice the distinct cap) entries counting repeats, then every identifier's size, and only then the distinct count, which hashes the identifiers and stops at the first card over `MAX_BATCH_ENTRIES`. The attach frame cap (`MAX_FRAME_LEN`, 16 MiB) is what bounds the request parsed before any of these run.

## The desktop

`useAgentCardsView` (`desktop/src/hooks/useAgentCardsView.ts`) holds the selection and the batch on screen:

- **What a batch reaches.** From a card's overlay or by voice: the card and its whole subtree (`cardSubtree`, from the same `cardTree` the view draws), whether or not the tree is open. From a selection: the selected cards. Selecting a closed card selects its subtree; an open card's checkbox is the card alone, because its children are on screen to pick (PRD #2374 decision of 2026-11-20, item 8).
- **Which actions are offered** (`batchChoices`): every action one of the cards offers, less Open, a reply and an answer to one agent's question (their ids and texts belong to one card). Whether each card gets it is the daemon's call in the preview.
- **Several daemons.** A selection can span decks; the hook asks each daemon process for its own preview and execution and shows the answers together. Each answer stays in a `CardBatchGroup` carrying its `deckId` and `instanceId`, and the dialog keeps that identity on every row: pane and agent ids are daemon-local, so a row's React key and the preview-name fallback for a nameless execution failure are deck + instance + pane + agent (#2399 review 1, audit S1). When a batch reaches more than one daemon, every row says which (`agent-cards-batch-daemon`, the dashboard's own `deckName`).
- **Late answers.** `batchRun` is bumped by every start, send and close, so a preview or execution that answers after the user closed or restarted the batch changes nothing.
- **A batch being sent cannot be dismissed or replaced** (#2399 review 2). `batchSending` is set in the same tick as Send and cleared when every daemon has answered. While it is set, `closeCardBatch` returns `BATCH_STILL_SENDING`, which the dialog shows (`agent-cards-batch-refusal`) and voice's `close` speaks through the overlay stack's close callback; Cancel is disabled; and a new batch (by voice, the only route while the modal is up) is refused with `ANOTHER_BATCH_SENDING`. So none of the dialog's ways out, and no second batch, throws away the per-agent results while the daemons are still acting. Switching the base screen away from the dashboard (`App.tsx`'s `base` leaving `overview` for the deck screen) unmounts the hook and its state with it; that is not guarded. Opening an agent from the dashboard does not unmount it: the agent's pane opens with `from: "overview"` as a sibling of the still-mounted `AgentOverview`.
- **A daemon that takes the execution and never answers** (#2399 re-review). The Tauri command `agent_card_invoke_batch` waits at most `card_batch_reply_timeout` (`desktop/src-tauri/src/lib.rs`): `DECK_REPLY_TIMEOUT` (15 s) for a preview, and that plus `CARD_BATCH_ENTRY_ALLOWANCE` (10 s) per entry for an execution, since the daemon runs entries one after another and answers once, after the last. When it runs out, a preview fails (it sent nothing), and an execution answers every entry it sent `unknown` with code `no_answer` (`CARD_BATCH_UNKNOWN`, `CODE_CARD_BATCH_NO_ANSWER`): the request reached the daemon, which may still be delivering, so neither `failed` ("nothing was sent") nor `delivered` is true. The daemon does not notice the client leaving until it writes its answer, so it keeps going. Nothing is retried, under the same request id or a new one; the dialog shows the rows as **No answer**, `batchSending` clears because the call settled, and the user decides from the cards whether to send again.

`AgentCardBatchDialog` shows the preview (`agent-cards-batch-confirm`), then the results (`agent-cards-batch-results`). `Escape` dismisses it through `VOICE_ACTIONS.closeCardBatch`, as `AgentCardOverlay` does, under the same sending guard. Its list scrolls and Cancel and Send stay in view at every width (Playwright `desktop/agent-cards/033`).

## Voice (CLAUDE.md rule 23)

Every way to start a batch is on voice, and every one of them only opens the confirmation: `sendCardBatch` is `no_voice` (PRD #802 D5, as `confirmCardAction`).

- `runCardActionBatch` (`run_card_action_batch`: `agent: agent_ref`, `action: card_action`) — a card and its whole subtree. Like `run_card_action` it resolves the action by its spoken label, here across the card and its subtree.
- `selectAgentCard` / `unselectAgentCard` (`select_agent_card` / `unselect_agent_card`: `agent: agent_ref`) — the same selection the checkboxes build, through `toggleCardSelected` with `want` set, so repeating "select the coder" does not undo it. A closed card takes its subtree along, as its checkbox does, and selecting from the table shows the Cards view. An `agent_ref` resolves against the agents voice reaches (the daemon the app shows, or every daemon under **All daemons**), so a selection across daemons is built under **All daemons** or by switching between utterances. A bare "select the coder" is `open_agent`'s (its description lists "select"); the two rows' descriptions send card / selection / batch wording here, the way `show_agent_card` takes "card".
- `runSelectionBatch` (`run_card_action_on_selection`: `action: card_action`) — the selection, with the spoken label matched against each selected card's buttons.

These were added in the #2399 review (suggestion 3): naming one card's subtree cannot reach an arbitrary set — unrelated roots, siblings without their parent, cards on several daemons — so a `no_voice` reason pointing at `run_card_action_batch` claimed an equivalence that does not hold. What stays off voice: `toggleCardSelecting` (the Select button only shows the checkboxes; voice's select turns selection on itself), `toggleCardSelected` (the checkbox flips; voice says which it wants), and `closeCardBatch`, reached by voice through `close` on the overlay stack.

## Not in this step

`when` conditions on actions (#2398) are not applied: a preview offers what is available today (an action is enabled, and a prompt action's agent is running). #2398 extends the preview's classification.

## Tests

| where | what |
| --- | --- |
| `src/agent_cards.rs` (`batch_tests`) | preview classification, request checks (the raw-entry budget included), partial failure and per-entry request ids, the wire's spelling |
| `src/daemon_client.rs` | `a_card_batch_is_sent_only_to_a_daemon_that_advertises_it` |
| `tests/e2e_agent_cards.rs` | `agent-report/batch/001–003`: preview against a real daemon, execution to PTY stand-ins, stale entries |
| `tests/e2e_agent_cards_live.rs` | `agent-report/live/003`: an interactive Haiku acting on a batch (lane 2) |
| `desktop/src-tauri/src/restart_binding_tests.rs` | `a_card_batch_the_deck_never_answers_ends_unknown_at_its_bound_and_is_not_resent`: a fake deck that takes the request and never answers; the execution ends `unknown` at its bound and is sent once, the preview fails at the round-trip bound |
| `desktop/src/components/AgentCardsBatch.test.tsx` | `desktop/agent-cards/028–031`: selection, overlay batch, confirmation, partial results; `034`: two daemons with colliding ids and a nameless failure; `037`: `Escape` dismisses a confirmation that is not being sent; `038`: an unanswered execution's rows read **No answer**, and the dialog closes and a new batch starts |
| `desktop/src/components/AgentCardsVoice.test.tsx` | `desktop/agent-cards/032`: voice opens the confirmation and sends nothing |
| `desktop/src/components/AgentCardsBatchVoice.test.tsx` | `desktop/agent-cards/035`: spoken `close`, `Escape` and Cancel are refused while a batch is sending; `036`: a selection built by voice; voice opens the confirmation without sending |
| `desktop/e2e/agent-cards-tree.spec.ts` | `desktop/agent-cards/033`: the confirmation's layout at five widths |
