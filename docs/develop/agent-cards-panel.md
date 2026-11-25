# The agent pane's Card panel

PR #2491's decision of 2026-11-25 removed the card overlay (`AgentCardOverlay.tsx`). A card now opens its agent's pane, and the pane shows the card in a **Card panel** beside the terminal. This page says how that is wired; the user-facing behaviour is in [`docs/desktop/agent-cards.md`](../desktop/agent-cards.md#the-card-panel).

## Where the state lives

The cards are the overview's (`useAgentCardsView` in `AgentOverview`), and the agent pane is the overview's **sibling** in `DeckShell` (`App.tsx`, `OverviewAgentPane`), so the two meet through a small store:

- `desktop/src/lib/paneCards.ts` — `createPaneCardsChannel()`, created once per shell. The overview publishes a `PaneCardsView` (`shownFor`, `cardFor`, `resultsFor`, `treeFor`, `endedFor`, `notice`, `controls`) in a layout effect; the pane reads it with `usePaneCards` (`useSyncExternalStore`), so only the pane re-renders when a card changes.
- The panel's open state is the **shell's** (`cardPanel` in `DeckShell`), settled once per navigation (`settleCardPanel`), keyed by the `requestedView` object a navigation creates: open when the view carries `cardPanel: true` (it came from a card, `+N`, the table's Show card or `show_agent_card`), or when the card needs the user, else as the previous pane had it, else the stored choice (`readCardPanelChoice`, `localStorage`, `try/catch`). A pane settled before the overview published anything (the app opening on a pane) is settled again in a layout effect, before paint; after that **only** `setCardPanel` (the header's Card button, voice's `open_card_panel` / `close_card_panel`) changes it. That rule is the point: the panel's size is the terminal's, and a terminal size change is a resize the agent sees, so card updates never toggle it (`095`, Playwright `097`).
- Under 900 px the panel stacks above the terminal at a **fixed** height (`flex: 0 0 min(40vh, 260px)`), not a content height: an auto height moved the terminal whenever the card's text changed (found by `097` at 375 px).
- `AgentTile` always wraps the terminal and the panel in `.agent-pane-body` (`display: contents` on a tile), so the terminal is never re-parented when the panel comes and goes.

## The review

The single-action review and the Close confirmation, which were the overlay's confirm step, are `AgentCardReviewDialog` — a dialog of its own on `OVERLAY_LAYER.dialog`, over the dashboard or the pane. `useAgentCardsView` holds it as `review` (`{ key, name, actionId, token, sent }`); `confirmCardAction` sends the reviewed token only, and after Send the dialog shows the result and closes with Close. `closeCardReview` is `no_voice`: voice's `close` reaches it through the fence it registers, as it reaches the batch confirmation. `runCardAction`'s `openPane` (the table menu's primary action) opens the agent's pane with its panel under the review.

## Navigation and keys

`stepAgentCard` moves from the pane's agent (`AgentOverview`'s `agentPane` prop) through the dashboard's placed order (`stepCard`) and opens the next agent's pane through the same `openAgent` path. `[`, `]` and `n` are handled in `DeckShell`'s pane `keydown` listener beside `Escape`, and only when the focused element is not inside `.agent-terminal-stack`, an `.xterm`, or a field; in the terminal they reach the agent (Playwright `098` asserts the bytes).

## Voice decisions

- `show_agent_card`, `next_needs_you_card` and `run_card_action` run on the dashboard and the agent pane; `next_agent_card` and `previous_agent_card` on the agent pane only.
- **"close the card" closes the panel** (`close_card_panel`), and `close_agent_card` was replaced rather than retired in favour of `close`: with the overlay gone, a user saying "close the card" while looking at the pane's panel means the panel, and "back to dashboard" / "close" keep leaving the pane. Retiring it would have sent "close the card" to `close`, which leaves the pane — more than was asked.
- `reply_to_agent_card` was retired with the reply box: its only target was that box. "open the coder" then "tell it …" (`dictate_to_agent`) types into the agent's terminal, unsent. The `reply_text` param kind (`ParamKind::ReplyText`) is now declared by no row; it is left in the crate for a follow-up to remove.
- The editor's placement row is `show_card_action_only_in_the_card_panel` (was `…_only_in_the_overlay`).

## The TUI

The TUI has no cards view, and its agent pane has no Card panel. Adding one would mean: subscribing the TUI to the card list (the daemon's `AttachRequest::ListAgentCards`, which the desktop already uses, and its change notifications), a ratatui panel widget beside the embedded terminal that renders an entry's two parts, key bindings for its actions that respect the TUI's command mode, the same "never resize by itself" rule for the PTY size, and a review step for agent-written text. That is a feature of its own, not a small change, so it is not part of PR #2491; the user docs name the gap ([In the TUI](../desktop/agent-cards.md#in-the-tui)).

## Tests

| where | what |
| --- | --- |
| `desktop/src/AgentPaneCardPanel.test.tsx` | `095` when the panel opens and that it never toggles on updates; `096` the header's moves and the keys |
| `desktop/src/components/AgentCardsVoice.test.tsx` | `008` the rows and screens; `094` the pane's voice rows |
| `desktop/src/components/AgentCardsContract.test.tsx`, `AgentCardsTree.test.tsx`, `AgentCardsReview.test.tsx`, `OverviewCards.test.tsx` | what the panel shows and does, through `src/test/cardPane.tsx` (the dashboard with the real panel beside it) |
| `desktop/e2e/agent-card-panel.spec.ts` | `097` placement at 375 and 1440 px and no resize on updates; `098` moves and keys; `099` the table menu; `100` icon hover marks, focus rings and tooltip placement |
| `desktop/src-tauri/src/voice/phrase_fixtures.toml` | the `card-panel-*` fixtures, run live by `voice_desktop_phrases_001` |
