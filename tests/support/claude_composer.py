#!/usr/bin/env python3
"""A scripted Claude Code screen for inbox/doorbell/056.

It draws what the deck's measured Claude Code profile reads: the start-up
header naming the release given as argv[3], the transcript's user cells, and
the empty composer between two full-width rules with its dim placeholder. Its
terminal is raw. A submitted line becomes a user cell, and every submitted
prompt runs the real `dot-agent-deck inbox fetch` (argv[2]) in the pane, as
Claude Code's prompt hook hands the agent its mail. No model runs.

Optional fixture knobs (unset by default):
  DOT_AGENT_DECK_TEST_COMPOSER_RELEASE overrides argv[3] in the header.
  DOT_AGENT_DECK_TEST_COMPOSER_REPORT=1 writes .spawn.json beside base:
    argv and the inherited VISUAL/EDITOR/CLAUDE_ENV_FILE (null means unset).
  DOT_AGENT_DECK_TEST_COMPOSER_BASH_TOOL=1 enables a Claude-style tool shell,
    sourcing CLAUDE_ENV_FILE before exec with stdio on the pane's tty.
    This models Claude-style sourcing only, not Codex's environment policy,
    and leaves the composer's own environment unchanged. Commands and env
    scripts must be trusted test code; this is not filesystem confinement.
    Use an isolated HOME/config/socket environment and leave BASH_ENV unset.

Tool mailbox contract (one publisher, one composer per base; no crash recovery):
  Publish {"id": "<unique nonempty id>", "argv": ["command", "..."]} by
  writing base.bash-tool.json.tmp-<unique>, closing it, then renaming it to
  base.bash-tool.json in the same directory. Never write the public name
  directly or overwrite a pending request. The poller first renames it to
  base.bash-tool.json.claimed-<pid>, then parses and removes that private file.
  After a claim the publisher may queue one next request at the public name.
  Results are closed temp files renamed to base.bash-tool-result.json:
  {"id": "<request id>", "exit": <status>}. Check the id, then acknowledge by
  removing that result; the poller claims no next request until it is absent.
  Invalid JSON/shape returns {"id": null, "exit": null, "error": "invalid_request"};
  a valid id is echoed even when argv is invalid, including NUL-bearing or
  OS-unencodable strings. Failure to start Bash returns
  error "exec_failed" with exit null. Acknowledge errors the same way.
"""
import json
import os
import select
import subprocess
import sys
import tempfile
import termios
import time
import tty

tty.setraw(0, termios.TCSANOW)
base, deck, version = sys.argv[1], sys.argv[2], sys.argv[3]
version = os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_RELEASE', version)
if os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_REPORT') == '1':
    report = {'argv': sys.argv, 'env': {
        name: os.environ.get(name)
        for name in ('VISUAL', 'EDITOR', 'CLAUDE_ENV_FILE')
    }}
    if os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_REPORT_NAMES') == '1':
        report['names'] = sorted(os.environ)
    with open(base + '.spawn.json', 'w') as output:
        json.dump(report, output)
with open(base + '.pid', 'w') as output:
    output.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
composer = bytearray()
transcript = []


def bash_tool():
    if os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_BASH_TOOL') != '1':
        return
    result_path = base + '.bash-tool-result.json'
    if os.path.exists(result_path):
        return  # The publisher has not acknowledged the preceding result.
    claimed = base + '.bash-tool.json.claimed-' + str(os.getpid())
    try:
        os.rename(base + '.bash-tool.json', claimed)
    except FileNotFoundError:
        return
    request_id = None
    try:
        try:
            with open(claimed) as request:
                command = json.load(request)
            if not isinstance(command, dict):
                raise ValueError('request must be an object')
            if not isinstance(command.get('id'), str) or not command['id']:
                raise ValueError('id must be a nonempty string')
            request_id = command['id']
            argv = command.get('argv')
            if not isinstance(argv, list) or not argv or not all(isinstance(arg, str) for arg in argv):
                raise ValueError('argv must be a nonempty string array')
            if any(b'\0' in os.fsencode(arg) for arg in argv):
                raise ValueError('argv must not contain NUL bytes')
        except (ValueError, UnicodeError):
            result = {'id': request_id, 'exit': None, 'error': 'invalid_request'}
        else:
            try:
                completed = subprocess.run([
                    'bash', '--noprofile', '--norc', '-c',
                    'if [[ -n ${CLAUDE_ENV_FILE:-} ]]; then source "$CLAUDE_ENV_FILE" || exit; fi; exec "$@"',
                    'composer-bash-tool', *argv,
                ])
                result = {'id': request_id, 'exit': completed.returncode}
            except OSError:
                result = {'id': request_id, 'exit': None, 'error': 'exec_failed'}
    finally:
        os.remove(claimed)
    with tempfile.NamedTemporaryFile(mode='w', dir=os.path.dirname(result_path),
                                     prefix=os.path.basename(result_path) + '.tmp-',
                                     delete=False) as output:
        temporary = output.name
        json.dump(result, output)
    os.replace(temporary, result_path)


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
    bash_tool()
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
