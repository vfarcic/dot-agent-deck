#!/usr/bin/python3
"""Private delayed wrapper for discovery tests; no daemon protocol injection."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

base = Path(os.environ['DAD_LIVE_SEGMENTS_BASE'])
# The wrapper resolves its own command normally. No native path is handed to
# it by the fixture; the daemon's independent authority is its spawn-time pin.
native = Path(shutil.which(os.environ['DAD_LIVE_SEGMENTS_KIND']))
mode = os.environ['DAD_DISCOVERY_LAUNCH']


def publish(suffix, value):
    target = base.with_suffix('.' + suffix)
    temporary = target.with_suffix(target.suffix + '.tmp')
    temporary.write_text(json.dumps(value))
    temporary.replace(target)


def wait(suffix):
    until = time.monotonic() + 90
    while not base.with_suffix('.' + suffix).exists():
        if base.with_suffix('.wrapper-stop').exists():
            sys.exit(0)
        if time.monotonic() >= until:
            raise RuntimeError('private launcher gate timed out: ' + suffix)
        time.sleep(.005)


def start():
    actual = native.parent / 'lookalike' / native.name if os.environ['DAD_DISCOVERY_LOOKALIKE'] == '1' else native
    # argv[0] deliberately claims the trusted native path even for the fake.
    return subprocess.Popen([str(native), '--live-segments-fixture'], executable=actual)


if sys.argv[1:] == ['intermediate']:
    child = start()
    publish('parent-ready', {'pid': os.getpid(), 'child': child.pid})
    wait('reparent-go')
    # The outer wrapper remains alive, but its native grandchild is orphaned.
    sys.exit(0)

publish('launcher-ready', {
    'pid': os.getpid(),
    'generation': int(os.environ.get('DOT_AGENT_DECK_EDITOR_GEN', '0')),
    'instance': str(Path(os.environ['VISUAL']).parent),
    'editors': {key: os.environ.get(key) for key in (
        'VISUAL', 'EDITOR', 'DOT_AGENT_DECK_EDITOR_GEN')},
})
wait('launch-go')
if mode == 'exec':
    actual = native.parent / 'lookalike' / native.name if os.environ['DAD_DISCOVERY_LOOKALIKE'] == '1' else native
    os.execve(actual, [str(native), '--live-segments-fixture'], os.environ)
elif mode == 'reparent':
    child = subprocess.Popen([sys.executable, __file__, 'intermediate'])
    child.wait()
    wait('wrapper-stop')
elif mode == 'spawn':
    child = start()
    child.wait()
    # Keep the PTY generation alive while testing an exited native candidate.
    wait('wrapper-stop')
else:
    raise RuntimeError('unknown private launcher mode: ' + mode)
