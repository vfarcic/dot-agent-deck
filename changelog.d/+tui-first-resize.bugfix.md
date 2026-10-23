## A new TUI pane's first resize reaches its agent

A pane resized right after it opened in the TUI could occasionally keep its agent at the old terminal size. The pane drew at the new size while the agent wrote at the old one, until the layout changed again. That first resize now reaches the agent.
