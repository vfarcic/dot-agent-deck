#!/usr/bin/env python3
"""A scripted Codex screen for inbox/codex/027.

It draws what the deck's measured Codex profile reads: the start-up header
naming the release given as argv[3] (`>_ OpenAI Codex (vX.Y.Z)`, `OpenAI` at
row 1, column 5), the logo whose top row starts at column cols/2 - 5 (the
width witness before a first prompt), the transcript's user rows (a dim `›`),
the composer (a bold `›`, the dim placeholder while it is empty, the cursor
after its text), the model row and the `? for shortcuts` footer, and the
update, folder-trust and hooks-review dialogs. Every frame is a full redraw
behind Codex's own redraw marker. Its terminal is raw. No model runs.

A submitted line becomes a user row and runs the real Codex prompt hook
(`<deck> hook --agent codex`, UserPromptSubmit with the line as its prompt)
in the pane, as Codex does, so the hook hands the agent its mail; the turn
then ends at once with Codex's Stop hook.

Files beside `base` (argv[1]) steer it, each read on every loop:
  .dialog         a dialog name (update, trust, hooks, other): draw it, and
                  send every byte to it instead of the composer
  .dialog_on_head open folder trust the moment the composer holds a whole
                  token head `(xxxxxx)`, in the same write as the head
  .keep           an Enter keeps the draft (Codex's paste-burst Enter)
  .swallow        an Enter empties the composer and submits nothing
  .extra          a transcript row to draw above the composer (dim `›`)
  .hidden         draw no composer (a screen that is not Codex's composer)
  .redraw         redraw once now (removed when done)
  .noheader       draw no start-up header box, as a resumed session's
                  screen does not show the one the deck reads
At 24 rows or fewer it draws no logo before the first prompt, as Codex does
at 24 x 80, so that frame has no width witness.
It writes .pid, .bytes (every byte read), .inputs (each read with its
CLOCK_MONOTONIC time), .dialog_bytes, .turns (each submitted line) and .hooks
(what the prompt hook printed).

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
    with open(base + '.spawn.json', 'w') as output:
        json.dump({'argv': sys.argv, 'env': {
            name: os.environ.get(name)
            for name in ('VISUAL', 'EDITOR', 'CLAUDE_ENV_FILE')
        }, 'names': sorted(os.environ)}, output)
# The generation's own session, the one `Deck::hook` names too: never
# another pane's or a predecessor's.
SESSION = '%s-session' % os.environ.get('DOT_AGENT_DECK_AGENT_ID', 'codex-composer-%d' % os.getpid())
with open(base + '.pid', 'w') as output:
    output.write(str(os.getpid()))
capture = open(base + '.bytes', 'ab', buffering=0)
inputs = open(base + '.inputs', 'a', buffering=1)
dialog_bytes = open(base + '.dialog_bytes', 'ab', buffering=0)
turns = open(base + '.turns', 'a', buffering=1)
composer = bytearray()
transcript = []
MARKER = b'\x1b[?2026h\x1b[1;1H\x1b[J'
DIALOGS = {
    'update': ('Update available! 0.164.0 -> 0.999.0',
               ['Update now (runs `npm install -g @openai/codex`)', 'Skip', 'Skip until next version']),
    'trust': ('Folder access', ['Yes, allow Codex to work in this folder', 'No, quit']),
    'hooks': ('Hooks need review', ['Trust and run these hooks', 'Review hooks', 'Disable these hooks']),
    'other': ('Something new', ['Continue', 'Quit']),
}


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


def flag(name):
    return os.path.exists(base + name)


def text_of(name):
    try:
        with open(base + name) as f:
            return f.read().strip()
    except FileNotFoundError:
        return None


def dialog_now():
    name = text_of('.dialog')
    return name if name in DIALOGS else None


def at(row, col):
    return ('\x1b[%d;%dH' % (row + 1, col + 1)).encode()


def redraw():
    cols, rows = os.get_terminal_size(0)
    out = bytearray(MARKER)
    name = dialog_now()
    if name:
        title, choices = DIALOGS[name]
        out += at(1, 2) + b'\x1b[1m' + title.encode() + b'\x1b[0m'
        for i, choice in enumerate(choices):
            row = 4 + i
            label = '› %d. %s' % (i + 1, choice) if i == 0 else '  %d. %s' % (i + 1, choice)
            label = label.ljust(cols)[:cols]
            if i == 0:
                # The highlighted choice, drawn to the right edge.
                out += at(row, 0) + b'\x1b[1;7m' + label.encode() + b'\x1b[0m'
            else:
                out += at(row, 0) + label.encode()
        out += at(4 + len(choices) + 1, 2) + b'Press enter to continue'
        out += b'\x1b[?2026l'
        os.write(1, bytes(out))
        return
    inner = cols - 2
    if not flag('.noheader'):
        out += at(0, 0) + ('╭' + '─' * inner + '╮').encode()
        header = (' >_ OpenAI Codex (v%s)' % version).ljust(inner)
        out += at(1, 0) + ('│' + header + '│').encode()
        out += at(2, 0) + ('│' + ' ' * inner + '│').encode()
        out += at(3, 0) + ('╰' + '─' * inner + '╯').encode()
    if not transcript and rows > 24:
        logo = cols // 2 - 5
        out += at(5, logo) + '⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿'.encode()
    row = 8
    rows_shown = list(transcript[-6:])
    extra = text_of('.extra')
    if extra:
        rows_shown.append(extra.encode())
    for line in rows_shown:
        out += at(row, 0) + b'\x1b[2m\xe2\x80\xba\x1b[0m ' + line
        row += 2
    if flag('.hidden'):
        out += at(row, 0) + b'Working (esc to interrupt)'
        out += b'\x1b[?2026l'
        os.write(1, bytes(out))
        return
    prompt_row = row
    out += at(prompt_row, 0) + b'\x1b[1m\xe2\x80\xba\x1b[0m '
    if composer:
        out += bytes(composer)
    else:
        out += b'\x1b[2mAsk Codex to do anything\x1b[0m'
    out += at(prompt_row + 2, 2) + b'gpt-test low \xc2\xb7 ~/work'
    if not composer:
        out += at(prompt_row + 3, 2) + b'? for shortcuts'
    shown = len(composer.decode('utf-8', 'replace'))
    out += at(prompt_row, 2 + shown)
    out += b'\x1b[?2026l'
    os.write(1, bytes(out))


def run_hook(event, extra):
    payload = {'hook_event_name': event, 'session_id': SESSION, 'cwd': os.getcwd()}
    payload.update(extra)
    result = subprocess.run([deck, 'hook', '--agent', 'codex'], input=json.dumps(payload).encode(),
                            capture_output=True, timeout=8)
    with open(base + '.hooks', 'a') as f:
        f.write(json.dumps({'event': event, 'exit': result.returncode, 'stdout': result.stdout.decode(),
                            'stderr': result.stderr.decode()}) + '\n')


def hook(line):
    # The prompt hook hands the agent its mail; with no model, the turn ends
    # at once with Codex's Stop hook.
    run_hook('UserPromptSubmit', {'prompt': line.decode('utf-8', 'replace')})
    run_hook('Stop', {})


def is_head(text):
    return (len(text) == 8 and text[:1] == b'(' and text[7:8] == b')'
            and all(0x61 <= c <= 0x7a for c in text[1:7]))


redraw()
geometry = os.get_terminal_size(0)
seen = (dialog_now(), text_of('.extra'), flag('.hidden'))
while True:
    bash_tool()
    current = os.get_terminal_size(0)
    state = (dialog_now(), text_of('.extra'), flag('.hidden'))
    if current != geometry or state != seen:
        geometry, seen = current, state
        redraw()
    if flag('.redraw'):
        os.remove(base + '.redraw')
        redraw()
    if not select.select([0], [], [], 0.025)[0]:
        continue
    chunk = os.read(0, 4096)
    if not chunk:
        break
    capture.write(chunk)
    inputs.write(json.dumps({'t': time.clock_gettime_ns(time.CLOCK_MONOTONIC),
                             'bytes': chunk.decode('latin-1')}) + '\n')
    if dialog_now():
        dialog_bytes.write(chunk)
        continue
    submitted = None
    for value in chunk:
        if value == 13:
            if flag('.keep'):
                continue
            line = bytes(composer)
            composer.clear()
            if line and not flag('.swallow'):
                submitted = line
        elif value == 21:
            composer.clear()
        elif value in (8, 127):
            if composer:
                composer.pop()
        elif value >= 32:
            composer.append(value)
    if flag('.dialog_on_head') and is_head(bytes(composer)):
        os.remove(base + '.dialog_on_head')
        with open(base + '.dialog', 'w') as f:
            f.write('trust')
        seen = (dialog_now(), text_of('.extra'), flag('.hidden'))
    if submitted is not None:
        transcript.append(submitted)
        turns.write(json.dumps({'time': time.monotonic(), 'line': submitted.decode('utf-8', 'replace')}) + '\n')
        redraw()
        hook(submitted)
    else:
        redraw()
