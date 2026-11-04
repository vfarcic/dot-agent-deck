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
import subprocess
import sys
import termios
import time

# Each measured release's executable, by SHA-256. A run measures whichever
# pinned release `claude` resolves to, and refuses any other executable.
PINS = {
    '2.1.310': 'b73676430af12f97316a798b8371f81d640f90a58bdcb8f3ce56a835bb47df56',
    '2.1.312': '4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d',
    # Official @anthropic-ai/claude-code-linux-x64@2.1.317 (tarball sha512
    # u19fYJ4l...XBlSA==), byte-identical to the installed 2.1.317.
    '2.1.317': 'eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0',
    # Official @anthropic-ai/claude-code-linux-x64@2.1.318 (tarball sha512
    # GX+7CUng...6NV2A==), byte-identical to the installed 2.1.318.
    '2.1.318': '708c77b204db30086ff6f1ed34577bda03e1be74ced80557cbf7b54249ebf0bd',
}
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
        self.lines, self.keys, self.restores = [], [], []
        self.frames = {}
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
        frame = {'text': self.text(), 'cursor': [self.screen.cursor.y, self.screen.cursor.x], 'off': len(self.raw)}
        self.frames[name] = frame
        return frame

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

    def draft_active(self):
        """Whether the composer holds a line as an editable draft.

        In 2.1.312's and 2.1.317's captures Claude draws an editable draft as
        `❯` and a no-break space, and a submitted line still on screen as `❯`
        and a plain space, so the two tell a restored prompt from the line
        that was just submitted.
        """
        prompt = next((i for i, row in enumerate(self.screen.display) if row.startswith('❯')), None)
        return (prompt is not None and self.screen.display[prompt].startswith('❯\xa0')
                and 'inboxfetch' in ''.join(self.composer().split()))

    def measure(self, kind):
        self.wait(lambda: any(row.startswith('❯') for row in self.screen.display), 40)
        self.pump(1)
        self.snap('empty')
        assert self.modes[-1]['bits'] == [False, False], self.modes
        if kind.startswith('menus'):
            for name, key in [('slash', '/'), ('at', '@'), ('bang', '!')]:
                self.send(key)
                self.pump(1)
                self.snap(name)
                self.send('\x1b')
                self.pump(.3)
                self.send('\x15')
                self.pump(.3)
            self.send('\x03')
            self.pump(.2)
            self.snap('exit-warning')
            self.pump(3)
            # This is a real transcript echo, blocked before any model turn.
            quotation = ('Quoted UI phrases: Your conversation moved to the background; '
                         'describe a task for a new session; Press Ctrl-C again to exit; '
                         '/help; @file; ! shell mode.')
            self.lines.append(quotation)
            self.send(quotation)
            self.pump(.5)
            self.send('\r')
            self.wait(lambda: (self.out / 'hook.jsonl').exists())
            self.pump(1)
            self.snap('quotation-echo')
            # Ctrl+U clears only the last visual line on a wrapped composer.
            # Remove the whole restored draft without submitting it again.
            for _ in quotation:
                self.send(BS)
                self.pump(.01)
            self.wait(lambda: not self.composer().strip())
            self.pump(.5)
            self.snap('transcript-quotation')
            self.send(L)
            self.pump(.3)
            if 'Press ← again to open agents' in self.text():
                self.send(L)
            self.wait(lambda: any(row.startswith('❯') and 'describe a task for a new session' in row
                                  for row in self.screen.display))
            self.pump(.5)
            self.snap('overview')
            return
        if kind == 'history':
            # Six prompts from an earlier session in this project are seeded
            # in the disposable HOME's history (`main`), and one line is
            # submitted here and blocked: a blocked prompt does not enter the
            # history (2.1.317, 2026-11-04). Up and Down then walk the history
            # from the empty composer, and the ways out of a recalled prompt
            # (Escape, Ctrl+U, Ctrl+C, Enter) are taken one at a time.
            def restored(line):
                prompt = next((i for i, row in enumerate(self.screen.display) if row.startswith('❯')), None)
                return (prompt is not None and self.screen.display[prompt].startswith('❯\xa0')
                        and line in ''.join(self.composer().split()))

            def hooks():
                path = self.out / 'hook.jsonl'
                return len(path.read_text().splitlines()) if path.exists() else 0

            line = 'HISTORYBLOCKEDgolf'
            self.lines.append(line)
            self.send(line)
            self.wait(lambda: line in self.text())
            self.pump(.3)
            self.snap('typed-1')
            self.points['mark:cr-1'] = len(self.raw)
            self.send('\r')
            self.wait(lambda: hooks() == 1)
            self.wait(lambda: restored(line), 5)
            self.pump(.5)
            self.snap('restored-1')
            self.send(K * 4)
            self.pump(.5)
            self.wait(lambda: not self.composer().strip(), 5)
            self.snap('cleared-1')
            for name, key in [('up-1', '\x1b[A'), ('up-2', '\x1b[A'), ('up-3', '\x1b[A'),
                              ('down-1', '\x1b[B'), ('down-2', '\x1b[B'), ('down-3', '\x1b[B')]:
                self.points[f'mark:{name}'] = len(self.raw)
                self.send(key)
                self.pump(1)
                self.snap(name)
            def recall(name):
                # Up recalls an entry. After Ctrl+U has cleared a recalled
                # prompt, Up has drawn nothing at all, three presses running
                # (2.1.317, 2026-11-04), so Down and Up are tried in turn and
                # the keys it took are recorded with the result.
                # Ctrl+U on a recalled prompt empties that history slot for
                # the session (2.1.317 and 2.1.318 then draw `History i/n`
                # over a blank box there), so Up is pressed until an
                # untouched, non-empty entry is drawn; every attempt is kept.
                sent = []
                for attempt in range(8):
                    self.send('\x1b[A')
                    sent.append('Up')
                    self.pump(1)
                    self.snap(f'recall-{name}-{attempt + 1}')
                    if 'History ' in self.text() and self.composer().strip():
                        break
                self.keys.append({'case': f'recall-{name}', 'keys': sent,
                                  'history': 'History ' in self.text(), 'composer': self.composer()})
                self.snap(f'recalled-before-{name}')

            for way, key in [('escape', '\x1b'), ('ctrl-u', K * 4), ('ctrl-c', '\x03')]:
                recall(way)
                self.points[f'mark:{way}'] = len(self.raw)
                self.send(key)
                self.pump(1.5)
                self.snap(f'after-{way}')
                self.send(K * 4)
                self.pump(.5)
                self.snap(f'reset-after-{way}')
            # A draft in the box, then Up: does Claude recall over a draft?
            self.send('DRAFThotel')
            self.wait(lambda: 'DRAFThotel' in self.text())
            self.pump(.3)
            self.snap('draft')
            self.send('\x1b[A')
            self.pump(1)
            self.snap('draft-up')
            self.send('\x1b[B')
            self.pump(1)
            self.snap('draft-up-down')
            self.send(K * 4)
            self.pump(.5)
            # The footer t042 saw under a recalled prompt: Shift+Tab cycles
            # the permission mode until the mode line reads manual mode.
            for i in range(6):
                if 'manual mode on' in self.text():
                    break
                self.send('\x1b[Z')
                self.pump(.8)
                self.snap(f'mode-{i}')
            recall('manual-mode')
            self.send(K * 4)
            self.pump(.5)
            self.snap('reset-after-manual-mode')
            # Enter on a recalled prompt submits it (blocked again).
            recall('enter')
            self.points['mark:enter'] = len(self.raw)
            self.send('\r')
            self.wait(lambda: hooks() == 2)
            self.pump(1.5)
            self.snap('after-enter')
            self.send(K * 4)
            self.pump(.5)
            self.snap('after-enter-cleared')
            # The emptied slot: recall a prompt, clear it with Ctrl+U while
            # it is recalled, then walk Up and Down across it. Earlier runs
            # drew `History i/n` over a blank box there (2.1.317 and 2.1.318,
            # 2026-11-04), the frame a profile must not read as empty.
            recall('slot')
            self.send(K * 4)
            self.pump(.8)
            self.snap('slot-cleared')
            for name, key in [('slot-up-1', '\x1b[A'), ('slot-down-1', '\x1b[B'), ('slot-down-2', '\x1b[B'),
                              ('slot-up-2', '\x1b[A'), ('slot-up-3', '\x1b[A'), ('slot-down-3', '\x1b[B'),
                              ('slot-down-4', '\x1b[B'), ('slot-down-5', '\x1b[B')]:
                self.points[f'mark:{name}'] = len(self.raw)
                self.send(key)
                self.pump(1)
                self.snap(name)
            self.send(K * 4)
            self.pump(.5)
            self.snap('end')
            return
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
            # The submitted line can stay on screen until Claude puts the
            # blocked prompt back as a draft (it does at 24 x 80), so the
            # restore is the change from the submitted form back to an
            # editable draft, not the line's text appearing.
            self.wait(lambda: not self.draft_active(), 5)
            self.wait(lambda: (self.out / 'hook.jsonl').exists()
                      and len((self.out / 'hook.jsonl').read_text().splitlines()) == i)
            hooked = time.monotonic_ns()
            # The blocked prompt comes back into the composer, on 2.1.317
            # once 629 ms after the hook, so a clear sent before it left the
            # line in the box: wait for it, bounded.
            try:
                self.wait(self.draft_active, 5)
                self.points[f'restored-{i:02}'] = len(self.raw)
                self.restores.append({'trial': i, 'ms_after_hook': (time.monotonic_ns() - hooked) / 1e6})
            except RuntimeError:
                self.restores.append({'trial': i, 'ms_after_hook': None})
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
        result = {'sha256': self.sha256, 'release': self.release, 'pid': pid, 'error': error, 'reads': self.reads,
                  'executable': self.executable, 'model': 'haiku',
                  'theme': self.theme, 'term': 'xterm-256color', 'colorterm': self.colorterm,
                  'events': self.events, 'modes': self.modes, 'keys': self.keys,
                  'restores': self.restores}
        (self.out / 'result.json').write_text(json.dumps(result, indent=2))
        (self.out / 'frames.json').write_text(json.dumps(self.frames, indent=2, ensure_ascii=False))


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
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest in PINS.values(), 'executable pin changed'
    release = subprocess.check_output([str(binary), '--version'], text=True).split()[0]
    expected = PINS[release]
    assert digest == expected, 'executable pin changed'
    recorder = Recorder(out, rows, cols)
    recorder.executable = str(binary)
    recorder.release, recorder.sha256 = release, expected
    menu_settings = {
        'menus': ('dark', None),
        'menus-light': ('light', None),
        'menus-truecolor-dark': ('dark', 'truecolor'),
        'menus-truecolor-light': ('light', 'truecolor'),
        'menus-dark-ansi': ('dark-ansi', None),
        'menus-light-ansi': ('light-ansi', None),
        'menus-dark-daltonized': ('dark-daltonized', None),
    }
    recorder.theme, recorder.colorterm = menu_settings[kind] if kind.startswith('menus') else ('dark', None)
    # Claude's own theme preference, confined to TuiDeck's disposable HOME.
    if kind.startswith('menus'):
        preferences = home / '.claude.json'
        preference_data = json.loads(preferences.read_text())
        preference_data['theme'] = recorder.theme
        preferences.write_text(json.dumps(preference_data))
    # Only the harness's disposable HOME is writable by this controller.
    settings = home / '.claude/settings.json'
    hook = ' '.join(shlex.quote(x) for x in [sys.executable, str(Path(__file__).resolve()), '--block-hook', str(out)])
    configuration = json.loads(settings.read_text()) if settings.exists() else {}
    if kind.startswith('menus'):
        # User settings override the legacy .claude.json theme preference.
        configuration['theme'] = recorder.theme
    configuration['autoUpdates'] = False
    configuration['hooks'] = {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': hook}]}]}
    settings.write_text(json.dumps(configuration))
    if kind == 'history':
        # Six prompts an earlier session in this project submitted, in the
        # shape Claude Code's own history log keeps them (one JSON object a
        # line: display, pastedContents, timestamp in ms, project, sessionId),
        # confined to the disposable HOME like the settings above.
        now = int(time.time() * 1000)
        with (home / '.claude/history.jsonl').open('a') as log:
            for age, display in [(360_000, 'HISTORYONEalpha'), (300_000, 'HISTORYTWObravo'),
                                 (240_000, 'HISTORYTHREEcharlie'), (180_000, 'HISTORYFOURdelta'),
                                 (120_000, 'HISTORYFIVEecho'), (60_000, 'HISTORYSIXfoxtrot')]:
                log.write(json.dumps({'display': display, 'pastedContents': {}, 'timestamp': now - age,
                                      'project': str(Path.cwd().resolve()),
                                      'sessionId': '00000000-0000-4000-8000-000000001666'}) + '\n')
    child, fd = pty.fork()
    if child == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        # M1's original clear environment advertised xterm-256color without
        # COLORTERM. TuiDeck advertises truecolor for its own outer screen.
        os.environ.pop('COLORTERM', None)
        if recorder.colorterm:
            os.environ['COLORTERM'] = recorder.colorterm
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
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == expected, 'executable pin changed'
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
