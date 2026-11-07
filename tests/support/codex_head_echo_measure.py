#!/usr/bin/env python3
"""Codex `head_echo` recorder (PRD #1666 part 3, Q27), run only by the
credentialed Rust TUI harness (`tests/e2e_codex_release_measure.rs`).

HOME, cwd and the deck's endpoints belong to TuiDeck; nothing outside its
disposable HOME is read or written. The one child, the deck's own
`dot-agent-deck wrap --agent codex -- codex …` (the shape a deck pane runs,
so the bytes read here are the bytes the daemon reads), is captured at fork
and reaped before returning. Every submitted line starts a real model turn on
the harness's imported login, which runs to its end before the next trial.

The deck's staged transaction is reproduced at its own timing: the token head
`(xxxxxx)` alone in one write; the rest of the line in one write as soon as the
composer holds exactly the head with the cursor after it (checked after every
PTY read, and at least every 10 ms, the transaction's tick); the Enter once
the composer holds exactly the whole line and no sooner than 150 ms after the
rest's write returned (`SUBMIT_DELAY`). Withdrawals send 0x7f, one per owned
character, as the deck does. pyte only schedules the writes; the Rust tests
replay the raw bytes through the deck's evidence state and screen profile and
decide every reading from those.

Usage: codex_head_echo_measure.py KIND OUT ROWS COLS DECK MODEL [TRIALS]
KIND is `staged` (TRIALS staged lines, each submitted), `staged-launcher`
(the same, Codex started through `sh -c`, which the wrapper cannot see into)
or `controls` (the withdrawal, Enter-timing and foreign-line controls). The
recorder waits for OUT/../go before it starts Codex, so the harness can
finish setting up the deck's hooks first; `go` may name the CODEX_HOME to use,
a directory inside the private HOME (default `$HOME/.codex`).
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import random
import re
import select
import shutil
import signal
import struct
import sys
import termios
import time

# The measured release's native executable, by SHA-256: the official
# `@openai/codex@0.164.0-linux-x64` package (tarball sha512 XLAoaWsG…l5Mg==),
# byte-identical to the one the host's npm install of 0.164.0 runs.
PINS = {
    '0.164.0': 'b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306',
}
TOKEN_ALPHABET = 'abcdfgimoprtvwxz'
ID_ALPHABET = 'abcdefghijklmnopqrstuvwxyz234567'
PREFIX, SUFFIX = ' dot-agent-deck: message ', ' waiting, run: dot-agent-deck inbox fetch'
DEL = '\x7f'
TICK = .01
ECHO_WAIT = 1.5
SUBMIT_DELAY = .15
SUBMIT_CONFIRM_WAIT = 1.0
PLACEHOLDERS = ('Ask Codex to do anything', 'Ask a follow-up question')
RNG = random.SystemRandom()
DEBUG_OUT = None


def token():
    return '(' + ''.join(RNG.choice(TOKEN_ALPHABET) for _ in range(6)) + ')'


def message_id():
    return 'm-%s-%016x' % (''.join(RNG.choice(ID_ALPHABET) for _ in range(8)), RNG.getrandbits(64))


def native_codex():
    """The native executable `codex` resolves to: itself, or the platform
    binary the npm launcher script spawns."""
    found = Path(shutil.which('codex')).resolve(strict=True)
    with found.open('rb') as head:
        script = head.read(2) == b'#!'
    if not script:
        return found, found
    root = found.parent.parent
    native = root / 'node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/bin/codex'
    return found, native.resolve(strict=True)


def running_codex(root, pin):
    """Every running descendant of `root` whose executable is named `codex`
    (the native binary, behind the wrapper and any launcher), with the SHA-256
    of the executable the process is running (`/proc/<pid>/exe`, so a file
    replaced or resolved differently after the start is not what is hashed).
    Fails unless there is at least one and each is the pinned executable."""
    parents = {}
    for entry in Path('/proc').iterdir():
        if not entry.name.isdigit():
            continue
        try:
            stat = (entry / 'stat').read_text()
        except OSError:
            continue
        # `pid (comm) state ppid …`: comm can hold spaces and parentheses.
        parents[int(entry.name)] = int(stat[stat.rindex(')') + 2:].split()[1])
    found, frontier = [], [root]
    while frontier:
        pid = frontier.pop()
        for child in (c for c, parent in parents.items() if parent == pid):
            frontier.append(child)
            try:
                exe = os.readlink('/proc/%d/exe' % child)
            except OSError:
                continue
            if Path(exe.removesuffix(' (deleted)')).name != 'codex':
                continue
            with open('/proc/%d/exe' % child, 'rb') as running:
                digest = hashlib.sha256(running.read()).hexdigest()
            found.append({'pid': child, 'exe': exe, 'sha256': digest})
    if not found or any(f['sha256'] != pin for f in found):
        raise RuntimeError('running Codex is not the pinned executable: %s' % found)
    return found


class Recorder:
    def __init__(self, out, rows, cols):
        self.out, self.rows, self.cols = out, rows, cols
        out.mkdir(mode=0o700)
        self.raw = bytearray()
        self.reads, self.points, self.events, self.modes = [], {}, [], []
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.lines, self.trials, self.controls, self.turns = [], [], [], []
        self.frames = {}
        self.fd = None
        self.launcher = False
        self.child, self.pin, self.running = None, None, None

    # -- the PTY -----------------------------------------------------------
    def text(self):
        return '\n'.join(row.rstrip() for row in self.screen.display)

    def read_once(self, timeout):
        flags = termios.tcgetattr(self.fd)[3]
        bits = [bool(flags & termios.ICANON), bool(flags & termios.ECHO)]
        if not self.modes or self.modes[-1]['bits'] != bits:
            self.modes.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'bits': bits})
        if not select.select([self.fd], [], [], max(0, timeout))[0]:
            return False
        data = os.read(self.fd, 65536)
        if not data:
            raise RuntimeError('Codex exited during measurement')
        self.reads.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'len': len(data)})
        self.raw.extend(data)
        self.stream.feed(data)
        os.write(1, data)
        return True

    def pump(self, duration):
        until = time.monotonic() + duration
        while time.monotonic() < until:
            self.read_once(min(TICK, until - time.monotonic()))

    def wait(self, predicate, seconds, what):
        """Read until `predicate` holds, checking it after every read and every
        tick. Returns the monotonic ns at which it first held."""
        until = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= until:
                raise RuntimeError('wait expired (%s): %s' % (what, self.text()))
            self.read_once(min(TICK, until - time.monotonic()))
        return time.monotonic_ns()

    def idle(self, seconds, hold=2.0):
        """Wait until Codex has shown its empty ready composer, with no
        running turn, for `hold` seconds straight (a turn's status row can
        drop out for a moment mid-turn). Returns when that run began, or None."""
        until = time.monotonic() + seconds
        since = None
        while time.monotonic() < until:
            if self.ready():
                since = since or time.monotonic_ns()
                if time.monotonic_ns() - since >= hold * 1e9:
                    return since
            else:
                since = None
            self.read_once(TICK)
        return None

    def poll(self, predicate, seconds):
        """`wait`, but a timeout returns None instead of failing the run."""
        try:
            return self.wait(predicate, seconds, 'poll')
        except RuntimeError:
            return None

    def send(self, text, label):
        t = time.monotonic_ns()
        data = text.encode()
        written = os.write(self.fd, data)
        self.events.append({'t': t, 'done': time.monotonic_ns(), 'send': text, 'label': label,
                            'off': len(self.raw), 'written': written})
        assert written == len(data), 'single-write trial was partial'
        return t

    def snap(self, name):
        self.points[name] = len(self.raw)
        self.frames[name] = {'text': self.text(), 'cursor': [self.screen.cursor.y, self.screen.cursor.x],
                             'off': len(self.raw)}

    # -- Codex's screen, as pyte sees it (scheduling only) ------------------
    def glyph(self, row):
        cell = self.screen.buffer[row][0]
        return cell.data == '›' and not cell.reverse

    def numbered(self, row):
        return re.match(r'\d+\.', self.screen.display[row][2:]) is not None

    def composer(self):
        """(text, cursor at its end) of the lowest `› ` row that is not a
        numbered choice, joined over its continuation rows; None when no such
        row is drawn."""
        rows = self.screen.display
        prompt = next((r for r in range(self.rows - 1, -1, -1)
                       if self.glyph(r) and not self.numbered(r)), None)
        if prompt is None:
            return None
        text, last = rows[prompt][2:].rstrip(), prompt
        for r in range(prompt + 1, self.rows):
            nxt = rows[r][2:].strip()
            if not nxt or not rows[r].startswith('  '):
                break
            text += ' ' + nxt
            last = r
        end = 2 + len(rows[prompt][2:].rstrip()) if last == prompt else len(rows[last].rstrip())
        cursor = (self.screen.cursor.y, self.screen.cursor.x)
        at_end = cursor == ((prompt, 2) if text in ('',) + PLACEHOLDERS else (last, end))
        return text, at_end

    def holds(self, text):
        c = self.composer()
        return c is not None and c[0] == text and c[1]

    def dialog(self):
        return any(self.screen.buffer[r][0].data == '›' and self.screen.buffer[r][0].reverse
                   and self.numbered(r) for r in range(self.rows))

    def busy(self):
        return 'esc to interrupt' in self.text()

    def ready(self):
        c = self.composer()
        return (c is not None and c[0] in PLACEHOLDERS and c[1] and not self.dialog()
                and not self.busy() and '? for shortcuts' in self.text())

    def user_row(self, line):
        """The line drawn as a transcript user message above the composer."""
        rows = self.screen.display
        prompt = next((r for r in range(self.rows - 1, -1, -1)
                       if self.glyph(r) and not self.numbered(r)), self.rows)
        for r in range(prompt):
            if rows[r].startswith('› '):
                text = rows[r][2:].rstrip()
                for n in range(r + 1, prompt):
                    if not rows[n].strip() or not rows[n].startswith('  '):
                        break
                    text += ' ' + rows[n].strip()
                if text == line:
                    return True
        return False

    def fresh_line(self):
        while True:
            line = token() + PREFIX + message_id() + SUFFIX
            if line[:8] not in self.text() and all(line[:8] != old[:8] for old in self.lines):
                return line

    # -- one staged ring, at the transaction's timing ----------------------
    def staged(self, n, line):
        head, rest = line[:8], line[8:]
        self.lines.append(line)
        trial = {'trial': n, 'line': line}
        self.trials.append(trial)
        if not self.ready():
            raise RuntimeError('trial %d: composer not empty and ready: %s' % (n, self.text()))
        self.points['mark:trial-%d' % n] = len(self.raw)
        trial['head_sent'] = self.send(head, 'head-%d' % n)
        seen = self.poll(lambda: self.holds(head) or self.dialog(), ECHO_WAIT)
        if seen is None or self.dialog():
            trial['head_seen'] = None
            raise RuntimeError('trial %d: no exact head echo within the echo wait: %s' % (n, self.text()))
        trial['head_seen'] = seen
        self.points['mark:rest-%d' % n] = len(self.raw)
        trial['rest_sent'] = self.send(rest, 'rest-%d' % n)
        rest_done = self.events[-1]['done']
        seen = self.poll(lambda: self.holds(line) or self.dialog(), ECHO_WAIT)
        if seen is None or self.dialog():
            raise RuntimeError('trial %d: no exact line echo within the echo wait: %s' % (n, self.text()))
        trial['line_seen'] = seen
        self.snap('typed-%02d' % n)
        floor = rest_done + int(SUBMIT_DELAY * 1e9)
        while time.monotonic_ns() < floor:
            self.read_once(min(TICK, (floor - time.monotonic_ns()) / 1e9))
        if not self.holds(line):
            raise RuntimeError('trial %d: the line changed before the Enter: %s' % (n, self.text()))
        self.points['mark:cr-%d' % n] = len(self.raw)
        trial['cr_sent'] = self.send('\r', 'cr-%d' % n)
        trial['user_row_seen'] = self.poll(lambda: self.user_row(line) and not self.holds(line),
                                           SUBMIT_CONFIRM_WAIT)
        self.snap('after-%02d' % n)
        trial['retained'] = self.holds(line) or (self.composer() or ('',))[0].startswith(line)
        # The turn runs to its end, the next trial needs Codex idle again.
        started = time.monotonic_ns()
        trial['busy_seen'] = self.poll(self.busy, 10)
        idle = self.idle(150)
        if idle is None:
            # A turn that does not end in time is interrupted, and recorded so.
            trial['interrupted'] = True
            self.send('\x1b', 'interrupt-%d' % n)
            idle = self.idle(30)
            if idle is None:
                raise RuntimeError('trial %d: Codex never idle again: %s' % (n, self.text()))
        trial['turn_ns'] = idle - started
        self.pump(1)
        self.snap('idle-%02d' % n)

    # -- the controls -------------------------------------------------------
    def typed_then_deleted(self, name, text, deletes=None):
        """Type `text` in one write, wait for its exact echo, then delete it
        with one 0x7f per character in one write and wait for the
        placeholder. The echo wait is the transaction's; a text that never
        echoes exactly is recorded, not retried."""
        if not self.ready():
            raise RuntimeError('%s: composer not empty and ready: %s' % (name, self.text()))
        entry = {'control': name, 'text': text}
        self.controls.append(entry)
        self.points['mark:%s' % name] = len(self.raw)
        entry['sent'] = self.send(text, name)
        entry['seen'] = self.poll(lambda: self.holds(text) or self.dialog(), ECHO_WAIT)
        entry['dialog'] = self.dialog()
        self.pump(.3)
        self.snap('typed:%s' % name)
        self.points['mark:deletes:%s' % name] = len(self.raw)
        count = len(text) if deletes is None else deletes
        entry['deletes'] = count
        entry['deletes_sent'] = self.send(DEL * count, 'deletes:%s' % name)
        entry['placeholder_seen'] = self.poll(self.ready, ECHO_WAIT)
        self.pump(.5)
        self.snap('deleted:%s' % name)
        if entry['placeholder_seen'] is None:
            raise RuntimeError('%s: no placeholder after the deletes: %s' % (name, self.text()))
        return entry

    def staged_head_then(self, name, line, rest_part):
        """The head alone, its exact echo, then `rest_part` (a strict prefix
        of the rest: the writer took only that much of the second stage), its
        echo, then the strict withdrawal of everything owned."""
        if not self.ready():
            raise RuntimeError('%s: composer not empty and ready: %s' % (name, self.text()))
        head = line[:8]
        owned = head + rest_part
        entry = {'control': name, 'text': owned, 'line': line}
        self.controls.append(entry)
        self.points['mark:%s' % name] = len(self.raw)
        entry['sent'] = self.send(head, name)
        entry['head_seen'] = self.poll(lambda: self.holds(head) or self.dialog(), ECHO_WAIT)
        if entry['head_seen'] is None:
            raise RuntimeError('%s: no exact head echo: %s' % (name, self.text()))
        if rest_part:
            self.points['mark:rest:%s' % name] = len(self.raw)
            entry['rest_sent'] = self.send(rest_part, 'rest:' + name)
        entry['seen'] = self.poll(lambda: self.holds(owned) or self.dialog(), ECHO_WAIT)
        entry['dialog'] = self.dialog()
        self.pump(.3)
        self.snap('typed:%s' % name)
        self.points['mark:deletes:%s' % name] = len(self.raw)
        entry['deletes'] = len(owned)
        entry['deletes_sent'] = self.send(DEL * len(owned), 'deletes:%s' % name)
        entry['placeholder_seen'] = self.poll(self.ready, ECHO_WAIT)
        self.pump(.5)
        self.snap('deleted:%s' % name)
        if entry['placeholder_seen'] is None:
            raise RuntimeError('%s: no placeholder after the deletes: %s' % (name, self.text()))

    def retained_on_enter(self, name, line):
        """The whole line in one write and the Enter straight after it, with
        no echo wait and no floor: what an Enter too soon does."""
        if not self.ready():
            raise RuntimeError('%s: composer not empty and ready: %s' % (name, self.text()))
        entry = {'control': name, 'text': line, 'delay_ms': None}
        self.controls.append(entry)
        self.points['mark:%s' % name] = len(self.raw)
        entry['sent'] = self.send(line, name)
        self.points['mark:cr:%s' % name] = len(self.raw)
        entry['cr_sent'] = self.send('\r', 'cr:' + name)
        self.after_enter(entry, name, line)

    def enter_timed(self, name, line, delay_ms):
        """The staged line at the transaction's timing, but its Enter sent
        `delay_ms` after the rest's write returned (or at the line's exact
        echo, if that came later) instead of the deck's 150 ms."""
        if not self.ready():
            raise RuntimeError('%s: composer not empty and ready: %s' % (name, self.text()))
        head, rest = line[:8], line[8:]
        entry = {'control': name, 'text': line, 'delay_ms': delay_ms}
        self.controls.append(entry)
        self.points['mark:%s' % name] = len(self.raw)
        entry['sent'] = self.send(head, name)
        if self.poll(lambda: self.holds(head), ECHO_WAIT) is None:
            raise RuntimeError('%s: no exact head echo: %s' % (name, self.text()))
        entry['rest_sent'] = self.send(rest, 'rest:' + name)
        floor = self.events[-1]['done'] + int(delay_ms * 1e6)
        entry['line_seen'] = self.poll(lambda: self.holds(line), ECHO_WAIT)
        if entry['line_seen'] is None:
            raise RuntimeError('%s: no exact line echo: %s' % (name, self.text()))
        while time.monotonic_ns() < floor:
            self.read_once(min(TICK, (floor - time.monotonic_ns()) / 1e9))
        self.points['mark:cr:%s' % name] = len(self.raw)
        entry['cr_sent'] = self.send('\r', 'cr:' + name)
        self.after_enter(entry, name, line)

    def after_enter(self, entry, name, line):
        """Submitted, or kept: a kept line (and the Enter, if Codex kept it as
        a newline) is deleted; a submitted one's turn runs to its end."""
        self.pump(1.5)
        self.snap('typed:%s' % name)
        composer = self.composer()
        entry['composer_after'] = None if composer is None else composer[0]
        entry['submitted'] = self.user_row(line) and not (composer or ('',))[0].startswith(line)
        if entry['submitted']:
            started = time.monotonic_ns()
            self.poll(self.busy, 10)
            idle = self.idle(150)
            if idle is None:
                entry['interrupted'] = True
                self.send('\x1b', 'interrupt:' + name)
                idle = self.idle(30)
                if idle is None:
                    raise RuntimeError('%s: Codex never idle again: %s' % (name, self.text()))
            entry['turn_ns'] = idle - started
            self.pump(1)
            self.snap('deleted:%s' % name)
            return
        self.points['mark:deletes:%s' % name] = len(self.raw)
        count = len(line) + 1
        entry['deletes'] = count
        entry['deletes_sent'] = self.send(DEL * count, 'deletes:%s' % name)
        entry['placeholder_seen'] = self.poll(self.ready, ECHO_WAIT)
        self.pump(.5)
        self.snap('deleted:%s' % name)
        if entry['placeholder_seen'] is None:
            raise RuntimeError('%s: no placeholder after the deletes: %s' % (name, self.text()))

    # -- the runs -----------------------------------------------------------
    def measure(self, kind, trials):
        self.wait(lambda: self.ready() or self.dialog(), 60, 'first composer')
        if self.dialog():
            raise RuntimeError('a dialog at start-up: ' + self.text())
        self.snap('empty')
        # The executable the composer belongs to, from the running process.
        self.running = running_codex(self.child, self.pin)
        assert self.modes[-1]['bits'] == [False, False], self.modes
        # The deck's first ring waits out the release's settle window (6.03 s
        # for 0.164.0) from the generation's first empty ready frame.
        self.pump(6.5)
        self.snap('settled')
        if self.launcher:
            # Nothing trusts the deck's hooks on this path, so Codex asks
            # about them; the user's answer here is to go on without them.
            self.wait(self.dialog, 10, 'the hooks review')
            if 'Hooks need review' not in self.text():
                raise RuntimeError('another dialog: ' + self.text())
            self.snap('hooks-review')
            self.points['mark:hooks-answer'] = len(self.raw)
            self.send('3', 'hooks-answer')
            if self.idle(10) is None:
                raise RuntimeError('no composer after the hooks review: ' + self.text())
            self.snap('answered')
        if kind == 'staged':
            for n in range(1, trials + 1):
                self.staged(n, self.fresh_line())
            return
        if kind != 'controls':
            raise RuntimeError('unknown kind ' + kind)
        # Complete-head withdrawal: the head alone, its echo, eight deletes.
        for n in range(1, 7):
            line = self.fresh_line()
            self.lines.append(line)
            self.typed_then_deleted('head-%d' % n, line[:8])
        # Partial-head: the writer took 1 to 7 bytes of the head.
        for k in range(1, 8):
            line = self.fresh_line()
            self.lines.append(line)
            self.typed_then_deleted('partial-%d' % k, line[:k])
        # The second stage's failure (Q24): the head echoed, then none, one,
        # ten or forty bytes of the rest taken, then the strict withdrawal of
        # what is owned.
        for m in (0, 1, 10, 40):
            line = self.fresh_line()
            self.lines.append(line)
            self.staged_head_then('second-%d' % m, line, line[8:8 + m])
        # A doorbell-shaped line the deck did not type (another token and
        # id), and an earlier attempt's line, each typed as a person would
        # paste it; neither is the deck's next line.
        foreign = self.fresh_line()
        self.lines.append(foreign)
        self.typed_then_deleted('foreign', foreign)
        self.typed_then_deleted('old-token', self.lines[0])
        # The Enter too soon: the line and the Enter with no echo wait.
        for n in range(1, 4):
            line = self.fresh_line()
            self.lines.append(line)
            self.retained_on_enter('enter-%d' % n, line)
        # The margin under the deck's floor: the staged line with its echo
        # awaited, and its Enter 50, 100 or 120 ms after the rest's write.
        for delay in (50, 100, 120):
            for n in range(1, 3):
                line = self.fresh_line()
                self.lines.append(line)
                self.enter_timed('enter-at-%d-%d' % (delay, n), line, delay)

    def save(self, pid, error, meta):
        (self.out / 'trace.bin').write_bytes(self.raw)
        index = {'source_run': self.out.name, 'argv0': 'codex', 'rows': self.rows, 'cols': self.cols,
                 'read_lens': [r['len'] for r in self.reads], 'points': self.points,
                 'edges': [], 'lines': self.lines}
        (self.out / 'index.json').write_text(json.dumps(index, indent=2))
        result = dict(meta, pid=pid, error=error, reads=self.reads, events=self.events, modes=self.modes,
                      trials=self.trials, controls=self.controls)
        (self.out / 'result.json').write_text(json.dumps(result, indent=2))
        (self.out / 'frames.json').write_text(json.dumps(self.frames, indent=2, ensure_ascii=False))


def load():
    """The box's contention when sampled: load averages and CPU pressure."""
    def read(path):
        try:
            return Path(path).read_text().strip()
        except OSError:
            return None
    return {'t': time.monotonic_ns(), 'loadavg': read('/proc/loadavg'), 'cpu_pressure': read('/proc/pressure/cpu'),
            'cpus': os.cpu_count()}


def rollout_user_messages(codex_home):
    """The text of every user message item Codex wrote to its rollout files,
    in file order, and the files read (relative to CODEX_HOME). The files
    themselves are copied beside the capture for inspection, never
    committed: they carry the model's turns."""
    found, files = [], []
    for path in sorted(codex_home.rglob('rollout-*.jsonl')):
        files.append(str(path.relative_to(codex_home)))
        if DEBUG_OUT is not None:
            shutil.copy(path, DEBUG_OUT / path.name)
        for raw in path.read_text(errors='replace').splitlines():
            try:
                entry = json.loads(raw)
            except ValueError:
                continue
            payload = entry.get('payload') or {}
            # 0.164.0 records a submitted prompt as a user `message` item
            # whose content is `input_text` (one more such item per session
            # carries Codex's own environment context).
            if (entry.get('type') == 'response_item' and payload.get('type') == 'message'
                    and payload.get('role') == 'user'):
                found.append(''.join(c.get('text', '') for c in payload.get('content', [])
                                     if c.get('type') == 'input_text'))
    return found, files


def main():
    global pyte
    import pyte
    kind, out = sys.argv[1], Path(sys.argv[2])
    rows, cols, deck, model = int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
    trials = int(sys.argv[7]) if len(sys.argv) > 7 else 0
    home = Path(os.environ['HOME']).resolve()
    assert home.name == 'home' and home.parent == Path.cwd().resolve(), 'requires TuiDeck private HOME'
    assert Path(os.environ['DOT_AGENT_DECK_ATTACH_SOCKET']).parent == home.parent, 'requires private endpoint'
    go = out.parent / 'go'
    deadline = time.monotonic() + 120
    while not go.exists():
        if time.monotonic() >= deadline:
            raise RuntimeError('the harness never said go')
        time.sleep(.05)
    # `go` names another CODEX_HOME inside the private HOME, or is empty.
    named = go.read_text().strip()
    codex_home = Path(named).resolve() if named else home / '.codex'
    assert codex_home.parent == home, 'requires a CODEX_HOME inside the private HOME'
    launcher, native = native_codex()
    digest = hashlib.sha256(native.read_bytes()).hexdigest()
    release = next((r for r, pin in PINS.items() if pin == digest), None)
    assert release is not None, 'executable pin changed: %s %s' % (native, digest)
    recorder = Recorder(out, rows, cols)
    recorder.launcher = kind.endswith('-launcher')
    codex = ['--model', model, '--sandbox', 'workspace-write', '--ask-for-approval', 'never',
             '-c', 'model_reasoning_effort="low"']
    if kind.endswith('-launcher'):
        # A launcher the wrapper cannot see into, the `devbox run codex-big`
        # shape: the wrapper adds no embedded-mode pair for it. It installs
        # and trusts nothing here only because this recorder removed the pane
        # id (`src/wrap.rs`, `installs_hooks`); in a production pane it
        # attempts the trust, which for `devbox run codex-big` fails to reach
        # Codex's app-server, and Codex starts with the hooks untrusted all
        # the same. This run's own Codex home has no trust records.
        argv = [deck, 'wrap', '--agent', 'codex', '--', 'sh', '-c', 'exec codex "$@"', 'sh'] + codex
        kind = kind[:-len('-launcher')]
    else:
        argv = [deck, 'wrap', '--agent', 'codex', '--', 'codex'] + codex
    meta = {'release': release, 'sha256': digest, 'launcher': str(launcher), 'executable': str(native),
            'argv': argv, 'model': model, 'codex_home': codex_home.name,
            'recorder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'term': 'xterm-256color',
            'colorterm': os.environ.get('COLORTERM'), 'kind': kind,
            'hooks_state': (codex_home / 'config.toml').read_text() if (codex_home / 'config.toml').exists()
            else None, 'load_start': load()}
    child, fd = pty.fork()
    if child == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        os.environ['TERM'] = 'xterm-256color'
        os.environ['LANG'] = 'C.UTF-8'
        os.environ['CODEX_HOME'] = str(codex_home)
        # The recorder's own pane identity is not Codex's: without it the
        # wrapper reports to no pane, and the harness's deck never reads this
        # recorder's pane as a Codex generation.
        for name in ('DOT_AGENT_DECK_PANE_ID', 'DOT_AGENT_DECK_AGENT_ID'):
            os.environ.pop(name, None)
        os.execv(deck, argv)
    recorder.fd, recorder.child, recorder.pin = fd, child, digest
    error = None
    try:
        recorder.measure(kind, trials)
    except Exception as exc:
        error = str(exc)
    finally:
        # The child stays ours until waitpid, so its PID cannot be recycled.
        os.kill(child, signal.SIGTERM)
        deadline = time.monotonic() + 3
        while os.waitpid(child, os.WNOHANG) == (0, 0):
            if time.monotonic() >= deadline:
                os.kill(child, signal.SIGKILL)
                os.waitpid(child, 0)
                break
            time.sleep(.02)
        os.close(fd)
        assert hashlib.sha256(native.read_bytes()).hexdigest() == digest, 'executable pin changed'
        meta['load_end'] = load()
        meta['running_executables'] = recorder.running
        global DEBUG_OUT
        DEBUG_OUT = out
        meta['rollout_user_messages'], meta['rollout_files'] = rollout_user_messages(codex_home)
        if not meta['rollout_files']:
            meta['codex_home_files'] = sorted(str(f.relative_to(codex_home)) for f in codex_home.rglob('*')
                                              if f.is_file())
        recorder.save(child, error, meta)


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        (Path(sys.argv[2]).parent / 'launcher-error.json').write_text(json.dumps(str(exc)))
        raise
