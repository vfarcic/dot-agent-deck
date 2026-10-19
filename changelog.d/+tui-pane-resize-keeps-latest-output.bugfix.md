## TUI: making a pane shorter keeps its latest output on screen

In the TUI, when a pane gets shorter, for example because you resize the terminal or change the layout, its newest lines stay visible and the oldest ones go into the pane's scrollback, as in most terminals. Previously the bottom of the pane was cut off, which could hide the agent's latest output and its prompt until it redrew. Making the pane taller again brings those lines back from scrollback.
