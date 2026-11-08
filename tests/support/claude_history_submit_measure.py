#!/usr/bin/env python3
"""Q143b recorder: the deck's line plus Enter after Ctrl+U on a recalled prompt.

Invoked only by the credentialed Rust TUI harness, which writes the committed
release recorder beside this file as `measure.py`. That recorder is imported,
not edited, so the file whose SHA-256 the committed fixtures cite stays the
same; its PINS, private blocking hook and Recorder are used as they are.

HOME, cwd and deck endpoints belong to TuiDeck. Submissions are blocked by the
private hook before Claude can start a paid model turn. The sole child is
captured at fork and reaped before returning.

Two cases, each from a slot k recalled with `ups` presses of Up and cleared
with `presses` presses of Ctrl+U in one write:
  plain     the plain rule over an empty box (blank, or the placeholder).
  labelled  then Up and Down back onto slot k: `History k/n` over an empty
            box (blank, or the placeholder), when Claude kept its position;
            otherwise nothing is rung.
In either, the deck's ring is copied: the whole line in one write, a wait for
its echo, then a lone CR. Afterwards the composer is cleared and the history
is walked Up and Down, and the disposable HOME's history log is read.
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import measure  # noqa: E402  (the committed release recorder, unchanged)

UP, DOWN, CTRL_U, BS = '\x1b[A', '\x1b[B', '\x15', '\x7f'
# Newest first, as Up recalls them; the seeding below writes them oldest first.
SEEDED = ['HISTORYSIXfoxtrot', 'HISTORYFIVEecho', 'HISTORYFOURdelta',
          'HISTORYTHREEcharlie', 'HISTORYTWObravo', 'HISTORYONEalpha']


def squash(text):
    return ''.join(text.split())


class SubmitRecorder(measure.Recorder):
    def drawn(self):
        """The row above the last `❯` row, and the composer below that row.

        The last `❯` row, not the first: a submitted line can stay on screen
        above the composer as a transcript user message.
        """
        display = self.screen.display
        prompt = max((i for i, row in enumerate(display) if row.startswith('❯')), default=None)
        if prompt is None:
            return None, None, False
        end = next((i for i in range(prompt + 1, self.screen.lines) if display[i].startswith('─')),
                   self.screen.lines)
        box = '\n'.join(row[2:].rstrip() for row in display[prompt:end]).strip('\n')
        rule = display[prompt - 1].rstrip() if prompt else ''
        return rule, box, display[prompt].startswith('❯\xa0')

    def record(self, name):
        rule, box, editable = self.drawn()
        self.snap(name)
        self.steps.append({'point': name, 'rule': rule, 'box': box, 'editable': editable,
                           'history': bool(rule) and rule.startswith('─── History ')})
        return rule, box

    def hooks(self):
        path = self.out / 'hook.jsonl'
        return len(path.read_text().splitlines()) if path.exists() else 0

    def premise(self, ok, what):
        if not ok:
            raise RuntimeError(f'premise differed: {what}; {self.text()}')

    def measure(self, case, ups, presses, line):
        self.steps = []
        self.reachable = None
        self.wait(lambda: any(row.startswith('❯') for row in self.screen.display), 40)
        self.pump(1)
        self.record('empty')
        self.premise(self.modes[-1]['bits'] == [False, False], f'raw input mode {self.modes}')
        k = len(SEEDED) + 1 - ups
        for i in range(1, ups + 1):
            self.points[f'mark:up-{i}'] = len(self.raw)
            self.send(UP)
            self.pump(1)
            rule, box = self.record(f'up-{i}')
        self.premise(rule.startswith(f'─── History {k}/6 ') and box == SEEDED[ups - 1],
                     f'slot {k}/6 recalled: {rule!r} {box!r}')
        # The person's Ctrl+U on the one-line recalled prompt: `presses`
        # presses in one write (the release recorder's `history` kind sent
        # four; one press drew the `Try "…"` placeholder in this recorder's
        # first run on 2.1.318, where four had drawn a blank box).
        self.points['mark:ctrl-u'] = len(self.raw)
        self.send(CTRL_U * presses)
        self.pump(1)
        rule, box = self.record('cleared')
        self.premise('History' not in rule and (box == '' or box.startswith('Try "')),
                     f'the plain rule over an empty box (blank or placeholder): {rule!r} {box!r}')
        if case == 'labelled':
            # Up then Down back onto slot k, when Claude kept its position.
            self.points['mark:slot-up'] = len(self.raw)
            self.send(UP)
            self.pump(1)
            self.record('slot-up')
            self.points['mark:slot-down'] = len(self.raw)
            self.send(DOWN)
            self.pump(1)
            rule, box = self.record('slot-down')
            # Empty as the plain case's box is: blank, or (after one press,
            # 2.1.317, 2026-11-04) the dim `Try "…"` placeholder.
            self.reachable = rule.startswith(f'─── History {k}/6 ') and (box == '' or box.startswith('Try "'))
            if not self.reachable:
                # Not the state this case measures: nothing is rung.
                self.record('end')
                return
        self.record('before-ring')
        # The ring: the whole line in one write (Claude's records are not
        # staged), its echo awaited, then the Enter on its own.
        self.lines.append(line)
        self.points['mark:line'] = len(self.raw)
        sent = time.monotonic_ns()
        self.send(line)
        self.wait(lambda: squash(line) in squash(self.drawn()[1] or ''), 5)
        self.echo_ns = time.monotonic_ns() - sent
        self.record('echoed')
        self.pump(.2)
        self.record('typed')
        self.points['mark:cr'] = len(self.raw)
        cr = time.monotonic_ns()
        self.send('\r')
        until = time.monotonic() + 10
        while self.hooks() == 0 and time.monotonic() < until:
            self.pump(.01)
        self.hook_ns = time.monotonic_ns() - cr if self.hooks() else None
        self.record('submitted')
        # The blocked line comes back as a draft (2.1.312, 2.1.317, 2.1.318):
        # first the submitted form, then the editable one. Neither wait fails
        # the run; what was drawn is recorded either way.
        until = time.monotonic() + 5
        while time.monotonic() < until and not (self.drawn()[2] is False or squash(line) not in squash(self.drawn()[1] or '')):
            self.pump(.01)
        until = time.monotonic() + 5
        while time.monotonic() < until and not (self.drawn()[2] and squash(line) in squash(self.drawn()[1] or '')):
            self.pump(.01)
        self.restore_ns = time.monotonic_ns() - cr
        self.pump(1)
        self.record('after-enter')
        # Clear the composer the way a wrapped draft needs (Ctrl+U clears only
        # its last visual line), one delete at a time.
        self.points['mark:clear'] = len(self.raw)
        for _ in range(len(line) + 4):
            self.send(BS)
            self.pump(.01)
        until = time.monotonic() + 5
        while time.monotonic() < until and (self.drawn()[1] or ''):
            self.pump(.05)
        self.pump(.5)
        self.record('after-clear')
        # The history afterwards: Up to the oldest entry and past it, then
        # Down back to the composer.
        for direction, key, count in [('walk-up', UP, 8), ('walk-down', DOWN, 9)]:
            for i in range(1, count + 1):
                self.points[f'mark:{direction}-{i}'] = len(self.raw)
                self.send(key)
                self.pump(1)
                self.record(f'{direction}-{i}')
        self.record('end')

    def save(self, pid, error):
        super().save(pid, error)
        path = self.out / 'result.json'
        result = json.loads(path.read_text())
        log = self.home / '.claude/history.jsonl'
        result.update({
            'recorder': 'claude_history_submit_measure.py',
            'case': self.case, 'ups': self.ups, 'presses': self.presses, 'line': self.line,
            'reachable': getattr(self, 'reachable', None),
            'steps': getattr(self, 'steps', []),
            'echo_ns': getattr(self, 'echo_ns', None), 'hook_ns': getattr(self, 'hook_ns', None),
            'restore_ns': getattr(self, 'restore_ns', None),
            'history_log': [json.loads(row)['display'] for row in log.read_text().splitlines()]
            if log.exists() else None,
            'load': self.load + [os.getloadavg(), Path('/proc/pressure/cpu').read_text().splitlines()[0]
                                 if Path('/proc/pressure/cpu').exists() else None],
        })
        # The release recorder's save has already written a result.json
        # without these fields, so the harness waits for `done`, written only
        # once the full result has replaced it.
        staged = self.out / 'result.json.part'
        staged.write_text(json.dumps(result, indent=2, ensure_ascii=False))
        os.replace(staged, path)
        (self.out / 'done').write_text('')


def main():
    out = Path(sys.argv[2])
    if sys.argv[1] == '--block-hook':
        measure.block_hook(out)
        return
    import pyte
    measure.pyte = pyte
    case, rows, cols = sys.argv[1], int(sys.argv[3]), int(sys.argv[4])
    ups, presses, line = int(sys.argv[5]), int(sys.argv[6]), sys.argv[7]
    assert case in ('plain', 'labelled') and 1 <= ups <= 5 and 1 <= presses <= 4, (case, ups, presses)
    home = Path(os.environ['HOME']).resolve()
    assert home.name == 'home' and home.parent == Path.cwd().resolve(), 'requires TuiDeck private HOME'
    assert Path(os.environ['DOT_AGENT_DECK_ATTACH_SOCKET']).parent == home.parent, 'requires private endpoint'
    binary = Path(shutil.which('claude')).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest in measure.PINS.values(), 'executable pin changed'
    release = subprocess.check_output([str(binary), '--version'], text=True).split()[0]
    expected = measure.PINS[release]
    assert digest == expected, 'executable pin changed'
    recorder = SubmitRecorder(out, rows, cols)
    recorder.executable, recorder.release, recorder.sha256 = str(binary), release, expected
    recorder.theme, recorder.colorterm = 'dark', None
    recorder.home, recorder.case, recorder.ups, recorder.presses, recorder.line = home, case, ups, presses, line
    recorder.load = [os.getloadavg(), Path('/proc/pressure/cpu').read_text().splitlines()[0]
                     if Path('/proc/pressure/cpu').exists() else None]
    # Only the harness's disposable HOME is writable by this controller.
    settings = home / '.claude/settings.json'
    hook = ' '.join(shlex.quote(x) for x in [sys.executable, str(Path(__file__).resolve()), '--block-hook', str(out)])
    configuration = json.loads(settings.read_text()) if settings.exists() else {}
    configuration['autoUpdates'] = False
    configuration['hooks'] = {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': hook}]}]}
    settings.write_text(json.dumps(configuration))
    # The six prompts the release recorder's `history` kind seeds, in the
    # same shape, confined to the disposable HOME.
    now = int(time.time() * 1000)
    with (home / '.claude/history.jsonl').open('a') as log:
        for age, display in zip([360_000, 300_000, 240_000, 180_000, 120_000, 60_000], reversed(SEEDED)):
            log.write(json.dumps({'display': display, 'pastedContents': {}, 'timestamp': now - age,
                                  'project': str(Path.cwd().resolve()),
                                  'sessionId': '00000000-0000-4000-8000-000000001666'}) + '\n')
    child, fd = pty.fork()
    if child == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        os.environ.pop('COLORTERM', None)
        os.environ['TERM'] = 'xterm-256color'
        os.environ['LANG'] = 'C.UTF-8'
        os.environ['DISABLE_AUTOUPDATER'] = '1'
        os.execv(str(binary), [str(binary), '--model', 'haiku', '--setting-sources', 'user'])
    recorder.fd = fd
    error = None
    try:
        recorder.measure(case, ups, presses, line)
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
