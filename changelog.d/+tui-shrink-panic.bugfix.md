## The TUI no longer crashes when the terminal is made smaller

Making the terminal window smaller could occasionally crash the TUI with an `index outside of buffer` error, when the resize landed while the TUI was drawing the screen. The TUI now draws that screen at the new size instead. The crash was possible in every release since v0.29.0.
