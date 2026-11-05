#!/usr/bin/env python3
"""An npm-shaped, interpreter-hosted agent entrypoint for discovery tests.

It stands in for `@openai/codex`'s `bin/codex.js` (`#!/usr/bin/env node`),
which `npm install -g` reaches through a symlink on PATH and which `spawn()`s
the native Codex binary as its child, passing its own arguments through.
Python stands in for node, which the test hosts are not guaranteed to have.
The native binary is not on the PATH the pane is spawned with: only this
script is. No daemon protocol injection.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

package = Path(__file__).resolve().parent.parent
kind = os.environ['DAD_LIVE_SEGMENTS_KIND']
# Beside the package, as npm's platform package is: never resolved by name.
native = package.parent / kind
variant = os.environ['DAD_HOSTED_VARIANT']
args = sys.argv[1:]
genuine = package / 'lib' / 'entry.py'
other_script = package / 'other' / 'entry.py'
other_interpreter = package / 'other' / 'python3'
interpreter = Path(os.readlink('/proc/self/exe'))

# The two controls hand the same process over before anything else runs,
# keeping the argv shape `env` gives the genuine one (`python3 <script>
# <args>`): to a byte-identical copy of this script under the same
# interpreter, or to this script under another interpreter (a copy of the
# same python3 at another inode).
if variant == 'other-script' and Path(__file__).resolve() == genuine:
    os.execv(interpreter, ['python3', str(other_script), *args])
if variant == 'other-interpreter' and interpreter != other_interpreter:
    os.execv(other_interpreter, ['python3', str(genuine), *args])

# `codex app-server` and `codex --version` pass straight through, as
# codex.js passes everything through: they are not the pane's agent.
probe = 'app-server' in args or '--version' in args
base = Path(os.environ['DAD_LIVE_SEGMENTS_BASE'])


def publish(suffix, value):
    target = base.with_suffix('.' + suffix)
    temporary = target.with_suffix(target.suffix + '.tmp')
    temporary.write_text(json.dumps(value))
    temporary.replace(target)


if not probe:
    publish('launcher-ready', {
        'pid': os.getpid(),
        'generation': int(os.environ.get('DOT_AGENT_DECK_EDITOR_GEN', '0')),
        'instance': str(Path(os.environ['VISUAL']).parent),
        'spawn_path': os.environ['PATH'],
        'argv': sys.argv,
        'editors': {key: os.environ.get(key) for key in (
            'VISUAL', 'EDITOR', 'DOT_AGENT_DECK_EDITOR_GEN')},
    })
    # Delayed only so a test can open its discovery barrier first.
    until = time.monotonic() + 90
    while not base.with_suffix('.launch-go').exists():
        if base.with_suffix('.wrapper-stop').exists():
            sys.exit(0)
        if time.monotonic() >= until:
            raise RuntimeError('private entrypoint gate timed out: launch-go')
        time.sleep(.005)

env = dict(os.environ)
env['CODEX_MANAGED_BY_NPM'] = '1'
env['CODEX_MANAGED_PACKAGE_ROOT'] = str(package)
child = subprocess.Popen([str(native), *args], env=env)
sys.exit(child.wait())
