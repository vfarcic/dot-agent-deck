"""The e2e build's CPR stand-in (`inbox/probe/055`).

The doorbell stand-in's screen and input handling (`tests/support/inbox_doorbell.rs`'s
`pane_with`), selected under the profile `e2e-standin-cpr`, except on a size
change: it asks for the cursor position (`ESC [ 6 n`), as Codex's input loop
does, gets no answer from a daemon no client is attached to, and redraws 2 s
later at the size it has then, so size changes that arrive during the wait
share one redraw. Input that arrives meanwhile waits, as it would behind a
blocked input loop.

Modes, from the environment:
- `CPR_STANDIN_NO_REDRAW=1`: a size change is asked about and never redrawn.
- `CPR_STANDIN_LATE_WITNESS=1`: until the first size change, the screen has no
  full-width row, so the deck has no width witness from start-up.

Usage: python3 cpr_composer.py <base>; writes <base>.pid, .bytes and .turns as
the doorbell stand-in does.
"""

import json
import os
import select
import sys
import termios
import time
import tty

WAIT = 2.0

tty.setraw(0, termios.TCSANOW)
base = sys.argv[1]
no_redraw = os.environ.get("CPR_STANDIN_NO_REDRAW") == "1"
witnessed = os.environ.get("CPR_STANDIN_LATE_WITNESS") != "1"
with open(base + ".pid", "w") as f:
    f.write(str(os.getpid()))
capture = open(base + ".bytes", "ab", buffering=0)
turns = open(base + ".turns", "a", buffering=1)
line = bytearray()
escape = bytearray()
string = bytearray()
taken = []


def redraw():
    cols = geometry.columns
    content = bytes(line) if line else b"\x1b[2m<empty>\x1b[22m"
    width = b"W" * cols if witnessed else b"WAITING-FOR-A-SIZE-CHANGE"
    frame = (
        b"\x1b[2J\x1b[HDOORBELL-STANDIN-READY\r\n"
        + width
        + b"\r\n\r\n"
        + b"".join(b"SUBMITTED:" + t + b"\r\n" for t in taken[-5:])
        + b"COMPOSER: "
        + content
    )
    os.write(1, frame)


def size_changed():
    global geometry, witnessed
    os.write(1, b"\x1b[6n")
    if no_redraw:
        geometry = os.get_terminal_size(0)
        return
    time.sleep(WAIT)
    geometry = os.get_terminal_size(0)
    witnessed = True
    redraw()


# The size the ready screen is drawn at, sampled before that screen is
# published: a resize the test makes once it sees the screen is then always
# an edge from this size, never taken as the baseline.
geometry = os.get_terminal_size(0)
redraw()
while True:
    if os.get_terminal_size(0) != geometry:
        size_changed()
        continue
    if not select.select([0], [], [], 0.025)[0]:
        continue
    chunk = os.read(0, 4096)
    if not chunk:
        break
    capture.write(chunk)
    for c in chunk:
        if string:
            # OSC, DCS, APC and PM run to BEL or ESC backslash, as in a terminal.
            if c == 7 or (string[-1] == 27 and c == 0x5C):
                string.clear()
            else:
                string.append(c)
        elif escape:
            escape.append(c)
            if len(escape) == 2 and c in (0x5D, 0x50, 0x5F, 0x5E):
                escape.clear()
                string.append(c)
            elif len(escape) == 2 and c != 0x5B and c != 0x4F:
                escape.clear()
            elif len(escape) >= 3 and 0x40 <= c <= 0x7E:
                escape.clear()
        elif c == 27:
            escape.append(c)
        elif c == 13:
            turns.write(json.dumps({"time": time.monotonic(), "line": line.decode("utf-8", "replace")}) + "\n")
            taken.append(bytes(line))
            line.clear()
        elif c == 21:
            line.clear()
        elif c in (8, 127):
            if line:
                line.pop()
        else:
            line.append(c)
    redraw()
