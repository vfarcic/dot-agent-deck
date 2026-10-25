#!/usr/bin/env python3
"""A scripted Claude Code screen for inbox/doorbell/056.

It draws what the deck's measured Claude Code profile reads: the start-up
header naming the release given as argv[3], the transcript's user cells, and
the empty composer between two full-width rules with its dim placeholder. Its
terminal is raw. A submitted line becomes a user cell, and every submitted
prompt runs the real `dot-agent-deck inbox fetch` (argv[2]) in the pane, as
Claude Code's prompt hook hands the agent its mail. No model runs.
"""
import json
import os
import select
import subprocess
import sys
import termios
import time
import tty

tty.setraw(0, termios.TCSANOW)
base, deck, version = sys.argv[1], sys.argv[2], sys.argv[3]
with open(base + '.pid', 'w') as output:
    output.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
composer = bytearray()
transcript = []


def redraw():
    cols = os.get_terminal_size(0).columns
    rule = ('─' * cols).encode()
    out = b'\x1b[2J\x1b[H\x1b[2;12HClaude Code v' + version.encode() + b'\r'
    for i, message in enumerate(transcript[-5:]):
        out += ('\x1b[%d;1H\x1b[48;5;237m❯ ' % (4 + i * 2)).encode() + message + b'\x1b[0m'
    out += b'\x1b[25;1H' + rule + '\x1b[26;1H❯ '.encode()
    if composer:
        out += bytes(composer)
    else:
        out += b'\x1b[2mTry "fix the bug"\x1b[22m'
    out += b'\x1b[27;1H' + rule + b'\x1b[28;3H? for shortcuts'
    out += ('\x1b[26;%dH' % (3 + len(composer.decode('utf-8', 'replace')))).encode()
    os.write(1, out)


redraw()
geometry = os.get_terminal_size(0)
escape = bytearray()
while True:
    current = os.get_terminal_size(0)
    if current != geometry:
        geometry = current
        redraw()
    if not select.select([0], [], [], 0.025)[0]:
        continue
    chunk = os.read(0, 4096)
    if not chunk:
        break
    capture.write(chunk)
    for value in chunk:
        if escape:
            escape.append(value)
            if len(escape) == 2 and value not in (0x5b, 0x4f):
                escape.clear()
            elif len(escape) >= 3 and 0x40 <= value <= 0x7e:
                escape.clear()
        elif value == 27:
            escape.append(value)
        elif value == 13:
            line = bytes(composer)
            composer.clear()
            if not line:
                continue
            turns.write(json.dumps({'time': time.monotonic(), 'line': line.decode('utf-8', 'replace')}) + '\n')
            transcript.append(line)
            redraw()
            result = subprocess.run([deck, 'inbox', 'fetch'], capture_output=True, timeout=8)
            with open(base + '.fetches', 'a') as f:
                f.write(json.dumps({'exit': result.returncode, 'stdout': result.stdout.decode(),
                                    'stderr': result.stderr.decode()}) + '\n')
        elif value == 21:
            composer.clear()
        elif value in (8, 127):
            if composer:
                composer.pop()
        elif value >= 32:
            composer.append(value)
    redraw()
