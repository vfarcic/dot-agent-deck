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

The answer rides on `AttachResponse.card_action_batch` (`CardBatchResult`: `request_id`, `entries`), one `CardBatchOutcome` per distinct entry, in the request's order. `outcome` is `included` / `skipped` for a preview and `delivered` / `closed` / `failed` for an execution. An included entry carries `name`, `label`, `kind`, the exact `prompt` with its `prompt_source` (absent for a close), and the `action_token` and `instance_id` to execute with. A skip or failure carries a stable `code` and a `message`. An entry that fails leaves the response's `ok` true; `ok: false` is reserved for a request refused as a whole (`check_batch`: a malformed request id, an empty or oversized action id, no entries, more than `MAX_BATCH_ENTRIES` = 256 distinct cards, an oversized identifier).

**The daemon advertises `agent-cards-batch`** (`CAP_AGENT_CARDS_BATCH`) on every platform. `DaemonClient::invoke_card_action_batch` is the one sender and sends only on a connection whose own Hello named it and `hello-follow-on`; it binds the connection to the `instance_id` the cards were listed by, as `invoke_card_action` does. `AgentCardsAnswer.batch` (set by `tree_gate`) tells the desktop whether to offer batches; the Tauri command is `agent_card_invoke_batch`, and the webview's runtime member is `invokeCardActionBatch(deckId, actionId, { mode, entries, requestId, instanceId })`.

**Rule 12 answer: no `PROTOCOL_VERSION` bump and no `.breaking.md`.** The request is a new verb every sender withholds until the daemon advertises it (rule 18's capability rung), the response field is additive and optional, and nothing about `invoke-card-action` or the card list changed meaning.

## Why preview, then execute

A batch is always confirmed, whatever each action's own `confirm` says (PRD #2374, #2399 scope), and the confirmation must show exactly the text each agent will get. Two calls make that hold:

- **preview** reads a fresh computation of the cards (`refresh`) and classifies each entry with `preview_entry` — the checks a single press makes in `invoke_once`, less the ones a preview cannot make. It sends nothing and remembers nothing. Skip codes are a single press's refusal codes: `unknown_entry` / `stale_entry` (no such card / the pane holds another agent), `unknown_action`, `client_side_action` (Open), `text_required` (a reply: the user's own words, one card at a time), `action_disabled`, and `agent_stopped` (`CODE_AGENT_STOPPED`, the `REASON_AGENT_STOPPED` word) for a prompt action whose agent is not running.
- **execute** takes only what the user confirmed, each entry with the `action_token` and `instance_id` its preview answered, and runs each through `invoke` — the single press's own path: the request-id claim and admission caps, the instance check (`daemon_replaced`), the token check (`stale_action`), delivery through `post_card_message`, and the record on that card's `recent_deliveries`. So an entry whose action changed after the preview fails on its own with `stale_action` and is sent neither text, and the entries after it still go (`execute_batch` does not stop on a refusal).

Each entry is invoked under its own request id, `<batch request_id>:<index>` (`batch_entry_request_id`), which is why the batch id is limited to 124 bytes: a repeat of the same execution repeats each entry's single press and is answered from the store, sending nothing again. `confirm` is `true` on every entry — the batch's confirmation is the user's confirmation of each — and `text` is always `None`, so no reply goes in a batch. Entries run one after another, not in parallel: each takes the card store's turn anyway, and a close waits for its process.

Duplicates are dropped before anything else (`distinct_batch_entries`: the first naming of a pane and agent wins), so the limit counts distinct cards and a card is never pressed twice by one batch.

## The desktop

`useAgentCardsView` (`desktop/src/hooks/useAgentCardsView.ts`) holds the selection and the batch on screen:

- **What a batch reaches.** From a card's overlay or by voice: the card and its whole subtree (`cardSubtree`, from the same `cardTree` the view draws), whether or not the tree is open. From a selection: the selected cards. Selecting a closed card selects its subtree; an open card's checkbox is the card alone, because its children are on screen to pick (PRD #2374 decision of 2026-11-20, item 8).
- **Which actions are offered** (`batchChoices`): every action one of the cards offers, less Open, a reply and an answer to one agent's question (their ids and texts belong to one card). Whether each card gets it is the daemon's call in the preview.
- **Several daemons.** A selection can span decks; the hook asks each daemon process for its own preview and execution and shows the answers together.
- **Late answers.** `batchRun` is bumped by every start, send and close, so a preview or execution that answers after the user closed or restarted the batch changes nothing.

`AgentCardBatchDialog` shows the preview (`agent-cards-batch-confirm`), then the results (`agent-cards-batch-results`). Its list scrolls and Cancel and Send stay in view at every width (Playwright `desktop/agent-cards/033`).

## Voice (CLAUDE.md rule 23)

`runCardActionBatch` is `voice: true`, with the `run_card_action_batch` row in `desktop/src-tauri/src/voice/commands.toml` (`agent: agent_ref`, `action: card_action`). Like `run_card_action` it resolves the action by its spoken label, here across the card and its subtree. It only opens the confirmation: `sendCardBatch` is `no_voice` (PRD #802 D5, as `confirmCardAction`). `toggleCardSelecting`, `toggleCardSelected` and `runSelectionBatch` are `no_voice` because they act on checkboxes the user points at; voice reaches the same set by naming the card whose subtree the batch covers. `closeCardBatch` is a dismissal of a dialog voice only opens.

## Not in this step

`when` conditions on actions (#2398) are not applied: a preview offers what is available today (an action is enabled, and a prompt action's agent is running). #2398 extends the preview's classification.

## Tests

| where | what |
| --- | --- |
| `src/agent_cards.rs` (`batch_tests`) | preview classification, request checks, partial failure and per-entry request ids, the wire's spelling |
| `src/daemon_client.rs` | `a_card_batch_is_sent_only_to_a_daemon_that_advertises_it` |
| `tests/e2e_agent_cards.rs` | `agent-report/batch/001–003`: preview against a real daemon, execution to PTY stand-ins, stale entries |
| `tests/e2e_agent_cards_live.rs` | `agent-report/live/003`: an interactive Haiku acting on a batch (lane 2) |
| `desktop/src/components/AgentCardsBatch.test.tsx` | `desktop/agent-cards/028–031`: selection, overlay batch, confirmation, partial results |
| `desktop/src/components/AgentCardsVoice.test.tsx` | `desktop/agent-cards/032`: voice opens the confirmation and sends nothing |
| `desktop/e2e/agent-cards-tree.spec.ts` | `desktop/agent-cards/033`: the confirmation's layout at five widths |
