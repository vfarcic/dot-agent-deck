#!/usr/bin/env python3
"""Synthetic editor contract for the e2e-only standin composer profile.

Select with DOT_AGENT_DECK_TEST_COMPOSER_PROFILE=standin and set
DOT_AGENT_DECK_TEST_COMPOSER_BINARY to this executable's absolute path;
the profile must match that executable path. Navigation is captured
verbatim but never becomes text. The renderer supplies erase-display + home,
a full-width witness, a dim empty placeholder, and a submitted transcript.
The input-mode reporter is Linux-only (SO_PEERCRED and /proc); the two
composer-profile lane-1 tests are cfg(target_os = "linux") and are not
compiled on other platforms. Missing python3 fails their explicit preflight.

Optional fixture knobs (unset by default):
  DOT_AGENT_DECK_TEST_COMPOSER_RELEASE appends " v<release>" to the ready
    header; the default header is unchanged.
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
import collections
import json
import os
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty

tty.setraw(0, termios.TCSANOW)
base = sys.argv[1]
version = os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_RELEASE')
if os.environ.get('DOT_AGENT_DECK_TEST_COMPOSER_REPORT') == '1':
    with open(base + '.spawn.json', 'w') as output:
        json.dump({'argv': sys.argv, 'env': {
            name: os.environ.get(name)
            for name in ('VISUAL', 'EDITOR', 'CLAUDE_ENV_FILE')
        }}, output)
with open(base + '.pid', 'w') as output:
    output.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
reports = open(base + '.mode', 'a', buffering=1)
line = bytearray()
escape = bytearray()
transcript = ''
revision = 0
seq = 0
capabilities = collections.OrderedDict()


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
    # These fixed cells and attributes are the test profile's public fixture
    # contract, not a recogniser for any production agent.
    content = bytes(line) if line else b'\x1b[2m<empty>\x1b[22m'
    header = b'DOORBELL-STANDIN-READY' + ((' v' + version).encode() if version is not None else b'')
    frame = (b'\x1b[2J\x1b[H' + header + b'\r\n'
             + ('W' * cols).encode() + b'\r\n'
             + transcript.encode() + b'\r\n'
             + ('PROCESSED:' + str(revision)).encode() + b'\r\n'
             + b'COMPOSER: ' + content)
    os.write(1, frame)


def mode_report():
    global seq
    sampled = time.monotonic_ns()
    flags = termios.tcgetattr(0)[3]
    done = time.monotonic_ns()
    seq += 1
    if done - sampled > 100_000_000:
        return
    try:
        with socket.socket(socket.AF_UNIX) as peer:
            peer.settimeout(0.5)
            peer.connect(os.environ['DOT_AGENT_DECK_SOCKET'])
            # The identity comes from this connected peer before the probe.
            # Linux-only reporting until the coder supplies the portable
            # production carrier; no guessed identity on other platforms.
            if not hasattr(socket, 'SO_PEERCRED'):
                return
            pid, _, _ = struct.unpack('3i', peer.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            with open('/proc/' + str(pid) + '/stat') as stat:
                start = stat.read().rsplit(')', 1)[1].split()[19]
            identity = (pid, start)
            request = {'pane_id': os.environ['DOT_AGENT_DECK_PANE_ID'],
                       'agent_id': os.environ['DOT_AGENT_DECK_AGENT_ID'],
                       'token': os.environ['DOT_AGENT_DECK_PANE_CAPABILITY']}
            stream = peer.makefile('rwb', buffering=0)
            if identity not in capabilities:
                probe = dict(request, message_type='hook_capabilities')
                stream.write((json.dumps(probe) + '\n').encode())
                answer = stream.readline()
                if not answer:
                    return
                capabilities[identity] = json.loads(answer).get('input_mode_report') == 1
                if len(capabilities) > 8:
                    capabilities.popitem(last=False)
            capabilities.move_to_end(identity)
            if not capabilities[identity]:
                return
            if time.monotonic_ns() - sampled > 100_000_000:
                return
            report = dict(request, message_type='input_mode', seq=seq,
                          icanon=bool(flags & termios.ICANON), echo=bool(flags & termios.ECHO),
                          sampled_mono_ns=sampled, sample_done_mono_ns=done)
            stream.write((json.dumps(report) + '\n').encode())
            answer = stream.readline()
            if answer:
                reports.write(json.dumps({'seq': seq, 'answer': answer.decode()}) + '\n')
    except (OSError, KeyError, ValueError):
        # No fabricated report or capability on failure. The daemon holds.
        return


redraw()
next_report = 0
geometry = os.get_terminal_size(0)
while True:
    bash_tool()
    if time.monotonic() >= next_report:
        mode_report()
        next_report = time.monotonic() + 1
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
            if len(escape) >= 3 and 0x40 <= value <= 0x7e:
                # CSI navigation changes the cursor in a real editor; on
                # this empty editor it changes no logical draft content.
                escape.clear()
        elif value == 27:
            escape.append(value)
        elif value == 13:
            text = line.decode('utf-8', 'replace')
            turns.write(json.dumps({'time': time.monotonic(), 'line': text}) + '\n')
            if text:
                transcript = 'SUBMITTED:' + text
            line.clear()
        elif value == 21:
            line.clear()
        elif value in (8, 127):
            if line:
                line.pop()
        else:
            line.append(value)
    revision += 1
    redraw()
