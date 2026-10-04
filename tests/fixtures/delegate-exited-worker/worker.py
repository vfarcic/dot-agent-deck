"""Stand-in worker for the `delegate-exited-worker` fixture.

Usage: python3 -u worker.py <name> [<work-done task>]

Appends every line it reads from its PTY to `./<name>-received.log`, tagged
with its pid, until `./<name>-exit` exists. Then it removes that trigger,
reports `work-done --task <work-done task>` through `$DAD_TEST_BIN` when one is
given, and exits 0 on its own.

The log is the tests' evidence that a task pointer reached the worker (issue
#1539). The pane's echo of the pointer is not: the daemon drops a pane's replay
ring whenever the pane is resized, and the TUI resizes every role pane from its
provisional spawn size once it lays out a freshly opened orchestration tab, so
a pointer delivered before that resize leaves no trace in the pane snapshot.
"""

import os
import select
import subprocess
import sys

name = sys.argv[1]
report = sys.argv[2] if len(sys.argv) > 2 else None
trigger = f"./{name}-exit"
received = f"./{name}-received.log"
fd = sys.stdin.fileno()

while not os.path.exists(trigger):
    readable, _, _ = select.select([fd], [], [], 0.1)
    if not readable:
        continue
    data = os.read(fd, 4096)
    if not data:
        break
    with open(received, "ab") as log:
        log.write(f"{os.getpid()}: ".encode() + data)

# Removed before the report, so a `clear = true` replacement running the same
# command waits for a NEW trigger instead of exiting at once.
if os.path.exists(trigger):
    os.remove(trigger)
if report is not None:
    subprocess.run([os.environ["DAD_TEST_BIN"], "work-done", "--task", report], check=False)
sys.exit(0)
