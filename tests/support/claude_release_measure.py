#!/usr/bin/env python3
"""M1/Q32 recorder, invoked only by the credentialed Rust TUI harness.

HOME, cwd and deck endpoints belong to TuiDeck. No host config is changed.
The sole child is captured at fork and reaped before returning. Submissions
are blocked by a private hook before Claude can start a paid model turn.
pyte schedules actions; the Rust tests replay the raw bytes through vt100.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import random
import select
import shlex
import shutil
import signal
import struct
import sys
import termios
import time

SHA = '4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d'
H, E, L, R, BS, D, K = '\x1b[H', '\x1b[F', '\x1b[D', '\x1b[C', '\x7f', '\x1b[3~', '\x15'
CASES = [
    ('reviewer', ['ab', '\n', '  ', H, K, H, BS, D, D, K]),
    ('join_text_bs_delete', ['ab', '\n', 'cd', H, K, BS, D]),
    ('join_blanks_bs_delete', ['  ', '\n', '  ', H, K, BS, D]),
    ('join_text_then_clear', ['ab', '\n', 'cd', H, K, E, K]),
    ('join_blanks_then_clear', ['  ', '\n', '  ', H, K, E, K]),
    ('home_end_left_right_kill', ['ab', '\n', 'cd', H, L, H, E, R, E, K]),
    ('ctrl_a_e_left_right_kill', ['ab', '\n', 'cd', '\x01', L, '\x01', '\x05', R, '\x05', K]),
    ('three_lines_join_kill', ['ab', '\n', '  ', '\n', 'cd', H, L, H, K, E, K]),
    ('blanks_partial_deletes', ['  ', '\n', '  ', H, L, BS, R, D]),
    ('blanks_home_kill_partial', ['  ', '\n', '  ', H, K, H, D, K]),
    ('linebreaks_partial_delete', ['\n', '\n', H, K, H, D, K]),
    ('blanks_enter_type_kill', ['   ', '\r', 'x', K]),
    ('breaks_enter_type_kill', ['\n', '\n', '\r', 'x', K]),
    ('mixed_blanks_enter_type_kill', ['  ', '\n', '  ', '\r', 'x', K]),
    ('blank_space_home', ['   ', H]),
    ('blank_break_home', ['\n', H]),
    ('space_delete_to_empty', [' ', H, D]),
    ('join_only_break', ['\n', K]),
    ('text_two_kills', ['ab', '\n', 'cd', K, K]),
]


def block_hook(out):
    payload = json.load(sys.stdin)
    with (out / 'hook.jsonl').open('a') as log:
        log.write(json.dumps({'t': time.monotonic_ns(), 'payload': payload}) + '\n')
    print(json.dumps({'decision': 'block', 'reason': 'release measurement; no model turn'}))


class Recorder:
    def __init__(self, out, rows, cols):
        self.out, self.rows, self.cols = out, rows, cols
        out.mkdir(mode=0o700)
        self.raw = bytearray()
        self.reads, self.points, self.edges, self.events, self.modes = [], {}, [], [], []
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.lines, self.keys = [], []
        self.fd = None

    def size(self, rows, cols):
        self.edges.append({'off': len(self.raw), 'rows': rows, 'cols': cols})
        self.events.append({'t': time.monotonic_ns(), 'resize': [rows, cols], 'off': len(self.raw)})
        self.screen.resize(rows, cols)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))

    def text(self):
        return '\n'.join(row.rstrip() for row in self.screen.display)

    def pump(self, duration):
        until = time.monotonic() + duration
        while time.monotonic() < until:
            flags = termios.tcgetattr(self.fd)[3]
            bits = [bool(flags & termios.ICANON), bool(flags & termios.ECHO)]
            if not self.modes or self.modes[-1]['bits'] != bits:
                self.modes.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'bits': bits})
            if not select.select([self.fd], [], [], min(.01, max(0, until - time.monotonic())))[0]:
                continue
            data = os.read(self.fd, 8192)
            if not data:
                raise RuntimeError('Claude exited during measurement')
            self.reads.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'len': len(data)})
            self.raw.extend(data)
            self.stream.feed(data)
            os.write(1, data)

    def wait(self, predicate, seconds=10):
        until = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= until:
                raise RuntimeError('measurement wait expired: ' + self.text())
            self.pump(.01)

    def send(self, text):
        self.events.append({'t': time.monotonic_ns(), 'send': text, 'off': len(self.raw)})
        data = text.encode()
        assert os.write(self.fd, data) == len(data), 'single-write trial was partial'

    def snap(self, name):
        self.points[name] = len(self.raw)
        return {'text': self.text(), 'cursor': [self.screen.cursor.y, self.screen.cursor.x], 'off': len(self.raw)}

    def reset(self):
        self.send('reset')
        self.pump(.3)
        self.send('\x03')
        self.pump(.6)

    def composer(self):
        prompt = next((i for i, row in enumerate(self.screen.display) if row.startswith('❯')), None)
        assert prompt is not None, self.text()
        # A following full-width rule bounds the multi-line editor.
        end = next((i for i in range(prompt + 1, self.screen.lines)
                    if self.screen.display[i].startswith('─')), self.screen.lines)
        return '\n'.join(row[2:].rstrip() for row in self.screen.display[prompt:end])

    def measure(self, kind):
        self.wait(lambda: any(row.startswith('❯') for row in self.screen.display), 40)
        self.pump(1)
        self.snap('empty')
        assert self.modes[-1]['bits'] == [False, False], self.modes
        if kind == 'startup':
            for sample in range(200):
                self.pump(.1)
                self.snap(f'idle-{sample:03}')
            self.snap('end')
            return
        if kind == 'keys':
            for name, keys in CASES:
                self.reset()
                start = self.snap(name + '-start')
                for key in keys:
                    self.send(key)
                    self.pump(.18)
                self.pump(.3)
                final = self.snap(name + '-final')
                self.send(R * 32)
                self.pump(.2)
                self.send('Z')
                self.pump(.3)
                self.send(L * 32)
                self.pump(.2)
                self.send('X')
                self.pump(.3)
                reveal = self.snap(name + '-reveal')
                self.send(L * 32)
                self.pump(.15)
                for _ in range(8):
                    self.send(E)
                    self.pump(.1)
                    self.send('|')
                    self.pump(.12)
                    row = self.screen.cursor.y
                    self.send(R)
                    self.pump(.12)
                    if self.screen.cursor.y == row:
                        break
                self.keys.append({'case': name, 'keys': keys, 'start': start,
                                  'final': final, 'reveal': reveal, 'composer': self.composer()})
            # The second live case's non-ASCII/backspace/blank-Enter premise.
            self.reset()
            self.send('  é')
            self.pump(.3)
            self.send(BS + '\x01')
            self.pump(.3)
            before = self.snap('blank-enter-before')
            before_composer = self.composer()
            self.send('\r')
            self.pump(2)
            after = self.snap('blank-enter-after')
            assert (before_composer, before['cursor']) == (self.composer(), after['cursor']), (before, after)
            self.send(E + 'Z' + H + 'X')
            self.pump(.3)
            self.keys.append({'case': 'blank-enter-nonascii', 'composer': self.composer()})
            # Two further live regressions check literal three-space input
            # and Left three times; measure both guards' premises directly.
            self.reset()
            self.send('   ')
            self.pump(.3)
            typed = self.snap('three-spaces-typed')
            self.send(L * 3)
            self.pump(.3)
            moved = self.snap('three-spaces-moved-back')
            assert typed['cursor'] == [moved['cursor'][0], moved['cursor'][1] + 3]
            self.send(E + 'Z' + H + 'X')
            self.pump(.3)
            self.keys.append({'case': 'three-spaces-left', 'composer': self.composer()})
            return
        rng = random.Random(1666 + self.cols)
        for i in range(1, 16):
            token = ''.join(rng.choice('abcdfgimoprtvwxz') for _ in range(6))
            line = f'({token}) dot-agent-deck: message m-q65abcde-{i:016x} waiting, run: dot-agent-deck inbox fetch'
            self.lines.append(line)
            self.points[f'mark:trial-{i}'] = len(self.raw)
            self.send(line)
            self.wait(lambda: 'inbox fetch' in self.text())
            self.pump(.2)
            self.snap(f'typed-{i:02}')
            self.pump(.15)
            self.points[f'mark:cr-{i}'] = len(self.raw)
            self.send('\r')
            self.wait(lambda: (self.out / 'hook.jsonl').exists()
                      and len((self.out / 'hook.jsonl').read_text().splitlines()) == i)
            self.pump(.5)
            self.snap(f'after-{i:02}')
            self.send(K * 4)
            self.pump(.4)
            self.snap(f'cleared-{i:02}')
        for i in range(1, 4):
            line = self.lines[i - 1]
            self.send(line)
            self.pump(.3)
            self.snap(f'dtyped-{i:02}')
            self.send(BS * len(line))
            self.pump(.5)
            self.snap(f'deleted-{i:02}')
        if self.cols == 200:
            for kind in ('resize', 'nudge'):
                for i in range(1, 11):
                    self.points[f'mark:{kind}-{i}'] = len(self.raw)
                    if kind == 'resize':
                        self.size(40, 160) if i % 2 else self.size(self.rows, self.cols)
                    else:
                        self.size(self.rows, self.cols - 1)
                        self.pump(.3)
                        self.size(self.rows, self.cols)
                    self.pump(.6)
                    self.snap(f'{kind}-{i:02}')

    def save(self, pid, error):
        (self.out / 'trace.bin').write_bytes(self.raw)
        index = {'source_run': self.out.name, 'argv0': 'claude', 'rows': self.rows, 'cols': self.cols,
                 'read_lens': [r['len'] for r in self.reads], 'points': self.points,
                 'edges': self.edges, 'lines': self.lines}
        (self.out / 'index.json').write_text(json.dumps(index, indent=2))
        result = {'sha256': SHA, 'pid': pid, 'error': error, 'reads': self.reads,
                  'executable': self.executable, 'model': 'haiku',
                  'events': self.events, 'modes': self.modes, 'keys': self.keys}
        (self.out / 'result.json').write_text(json.dumps(result, indent=2))


def main():
    global pyte
    out = Path(sys.argv[2])
    if sys.argv[1] == '--block-hook':
        block_hook(out)
        return
    import pyte
    kind, rows, cols = sys.argv[1], int(sys.argv[3]), int(sys.argv[4])
    home = Path(os.environ['HOME']).resolve()
    assert home.name == 'home' and home.parent == Path.cwd().resolve(), 'requires TuiDeck private HOME'
    assert Path(os.environ['DOT_AGENT_DECK_ATTACH_SOCKET']).parent == home.parent, 'requires private endpoint'
    binary = Path(shutil.which('claude')).resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == SHA, 'executable pin changed'
    recorder = Recorder(out, rows, cols)
    recorder.executable = str(binary)
    # Only the harness's disposable HOME is writable by this controller.
    settings = home / '.claude/settings.json'
    hook = ' '.join(shlex.quote(x) for x in [sys.executable, str(Path(__file__).resolve()), '--block-hook', str(out)])
    configuration = json.loads(settings.read_text()) if settings.exists() else {}
    configuration['autoUpdates'] = False
    configuration['hooks'] = {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': hook}]}]}
    settings.write_text(json.dumps(configuration))
    child, fd = pty.fork()
    if child == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        # M1's original clear environment advertised xterm-256color without
        # COLORTERM. TuiDeck advertises truecolor for its own outer screen.
        os.environ.pop('COLORTERM', None)
        os.environ['TERM'] = 'xterm-256color'
        os.environ['LANG'] = 'C.UTF-8'
        os.environ['DISABLE_AUTOUPDATER'] = '1'
        os.execv(str(binary), [str(binary), '--model', 'haiku', '--setting-sources', 'user'])
    recorder.fd = fd
    error = None
    try:
        recorder.measure(kind)
    except Exception as exc:
        error = str(exc)
    finally:
        # child remains ours until waitpid, so its PID cannot be recycled.
        os.kill(child, signal.SIGTERM)
        deadline = time.monotonic() + 2
        while os.waitpid(child, os.WNOHANG) == (0, 0):
            if time.monotonic() >= deadline:
                os.kill(child, signal.SIGKILL)
                os.waitpid(child, 0)
                break
            time.sleep(.02)
        os.close(fd)
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == SHA, 'executable pin changed'
        recorder.save(child, error)


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        if sys.argv[1] == '--block-hook':
            print('measurement hook failed closed', file=sys.stderr)
            sys.exit(2)
        (Path(sys.argv[2]).parent / 'launcher-error.json').write_text(json.dumps(str(exc)))
        raise
