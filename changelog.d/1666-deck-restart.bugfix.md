## After a deck restarts, its stopped agents are removed with one notice, and actions on them no longer reach a new agent

A restarted deck can run a new agent under the same name, or in the same pane, as an agent from before the restart. Acting on an agent you saw before the restart could reach that new agent instead: opening, typing into, closing, renaming or messaging it, answering its question, or a voice command. A terminal could also show the previous agent's output under the new agent's name.

Now, when the TUI or the desktop app reconnects to a restarted deck, it removes the cards and panes of the agents the restart stopped, and in the TUI an orchestration tab left with none of its agents, and shows one notice naming each of them with its role and orchestration: `The deck restarted; these agents were stopped: coder (worker in team), api-fix.` The notice stays until you dismiss it, and agents a later restart stops join it.

- **TUI:** the notice has a row of its own above the footer. On the dashboard or an orchestration tab, `a` lists every agent it names and `c` dismisses it; in a pane or a field, use the footer's `[Agents]` and `[Dismiss]` buttons.
- **Desktop app:** the notice sits in the corner, lists every agent, and closes with its `×`.

Removing them stops and deletes nothing: a dispatched unit's worktree and branch stay on disk. The restarted deck's agents get cards and terminals of their own, and none takes over a removed agent's name or output. An action on an agent from before the restart that was already under way, such as an open confirmation or a voice command, is not sent and says `that agent is gone (the deck restarted)`; text typed into a TUI pane while it was reconnecting is discarded rather than sent to the new agent. A pane that only lost its connection while the deck kept running still reconnects to its own agent.

In the TUI, the Quit, Close, Stop and configuration confirmations now ignore keys pressed before they appeared, and in a terminal too small to show one of their choices, the keys that would pick it do nothing.

An orchestration started while the TUI was reconnecting gets no tab or role cards in the TUI; detach and start the TUI again to see it, or look at the desktop app's Dashboard.

See [Troubleshooting → The deck restarted and its agents disappeared](https://agent-deck.devopstoolkit.ai/docs/troubleshooting.md#the-deck-restarted-and-its-agents-disappeared).
