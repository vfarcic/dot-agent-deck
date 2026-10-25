## Keys typed right after resizing the terminal take effect straight away

In the TUI, a key pressed just as the terminal window was resized could do nothing until you pressed another key, so the deck looked as if it had ignored it. Keys typed during or right after a resize now take effect at once, and a resize that arrives together with typing now redraws the deck at the new size instead of waiting for the next resize.
