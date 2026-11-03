#!/usr/bin/env python3
"""M59 recorder: a real Claude Code running the deck's editor helper.

Invoked only by `tests/e2e_claude_helper_alarm.rs`, in a pane of the TUI
harness, so HOME and cwd are the harness's own. Claude Code's VISUAL and
EDITOR name the helper copy of an instance the test created; the helper's
daemon socket is one the test process owns and never answers. The user's
editor, which the helper restores, is the stand-in in
`claude_editor_measure.py`. Nothing is submitted and a private hook blocks
any submission, so no model turn starts.

argv: <cell> <out> <helper> <socket> <generation> <editor-script>

Cells:
  armed-stall   arm present; the helper pauses at its first ready poll (the
                e2e pause seam, on a FIFO nobody writes), so only its 5 s
                alarm can end it
  armed-timeout arm present; no pause: the unanswered exchange runs out
  unarmed-long  arm absent; the user's editor sleeps 8 s, writes a sentinel
                and exits 0, ten times, then once exiting 37
"""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

PINS = {'2.1.317': 'eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0'}
TRIALS = 10
NOTICE = b'dot-agent-deck: the deck was checking this input box.'
# Claude's own line when the editor it ran ends badly, naming the signal or
# exit code it reaped (2.1.317: `<editor> closed unexpectedly (SIGALRM)`,
# `<editor> quit unexpectedly (exit code 37)`).
CLAUDE_EDITOR_NOTICE = re.compile(rb'(?:closed|quit) unexpectedly \([^)\r\n]{1,40}\)')


def claude_notices(window):
    return sorted({m.group(0).decode() for m in CLAUDE_EDITOR_NOTICE.finditer(window)})


def block_hook(out):
    payload = json.load(sys.stdin)
    with (out / 'hook.jsonl').open('a') as log:
        log.write(json.dumps({'t': time.monotonic_ns(), 'payload': payload}) + '\n')
    print(json.dumps({'decision': 'block', 'reason': 'M59 measurement; no model turn'}))


def proc_stat(pid):
    """(state, ppid, exit_code) from /proc/<pid>/stat, or None once reaped."""
    try:
        raw = Path(f'/proc/{pid}/stat').read_text()
    except OSError:
        return None
    fields = raw[raw.rindex(')') + 2:].split()
    # fields[0] is field 3 (state); exit_code is field 52.
    return fields[0], int(fields[1]), int(fields[49]) if len(fields) > 49 else None


def exe_of(pid):
    try:
        return os.readlink(f'/proc/{pid}/exe')
    except OSError:
        return None


class Run:
    def __init__(self, cell, out, helper, socket, generation, editor_script):
        self.cell, self.out, self.helper = cell, out, helper
        self.socket, self.generation = socket, generation
        self.editor_script = editor_script
        self.raw, self.reads, self.events, self.trials = bytearray(), [], [], []
        self.screen = pyte.Screen(200, 50)
        self.stream = pyte.ByteStream(self.screen)
        self.trace = out / 'helper-trace.log'
        self.editor_log = out / 'editor.jsonl'

    def text(self):
        return '\n'.join(row.rstrip() for row in self.screen.display)

    def composer(self):
        rows = self.screen.display
        prompt = next((i for i, row in enumerate(rows) if row.startswith('❯')), None)
        if prompt is None:
            return None
        end = next((i for i in range(prompt + 1, len(rows)) if rows[i].startswith('─')), len(rows))
        return '\n'.join(row[2:].rstrip() for row in rows[prompt:end])

    def pump(self, duration):
        until = time.monotonic() + duration
        while True:
            left = until - time.monotonic()
            if left <= 0:
                return
            if not select.select([self.fd], [], [], min(.001, left))[0]:
                continue
            data = os.read(self.fd, 65536)
            if not data:
                raise RuntimeError('Claude exited during measurement')
            self.reads.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'len': len(data)})
            self.raw.extend(data)
            self.stream.feed(data)

    def wait(self, predicate, seconds):
        until = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= until:
                return False
            self.pump(.005)
        return True

    def send(self, text, label):
        self.events.append({'t': time.monotonic_ns(), 'send': text, 'label': label, 'off': len(self.raw)})
        assert os.write(self.fd, text.encode()) == len(text.encode())

    def reset(self):
        self.pump(.3)
        self.send('\x1b', 'dismiss')
        self.pump(.1)
        self.send('reset', 'reset')
        self.pump(.15)
        self.send('\x03', 'clear')
        self.pump(.5)

    def helper_children(self):
        found = []
        for entry in os.listdir('/proc'):
            if not entry.isdigit():
                continue
            st = proc_stat(int(entry))
            if st and st[1] == self.child and exe_of(int(entry)) == self.helper:
                found.append(int(entry))
        return found

    def editor_entries(self):
        if not self.editor_log.exists():
            return []
        return [json.loads(x) for x in self.editor_log.read_text().splitlines() if x.strip()]

    def trial(self, i):
        self.reset()
        draft = f'M59_DRAFT_{i}'
        self.send(draft, 'draft')
        self.pump(.5)
        unarmed = self.cell == 'unarmed-long'
        rc = 37 if unarmed and i == TRIALS + 1 else 0
        (self.out / 'plan.json').write_text(json.dumps(
            {'action': 'write', 'text': f'M59_EDITED_{i}', 'sleep': 8, 'rc': rc}))
        editors_before = len(self.editor_entries())
        before_trace = self.trace.read_text() if self.trace.exists() else ''
        raw_before = len(self.raw)
        sent = time.monotonic_ns()
        self.send('\x07', 'ctrl-g')
        helper = None
        until = time.monotonic() + 5
        while helper is None and time.monotonic() < until:
            pids = self.helper_children()
            helper = pids[0] if pids else None
            if helper is None:
                self.pump(.001)
        seen = time.monotonic_ns()
        zombie_exit = None
        gone = None
        end_off = None
        if helper is not None:
            limit = time.monotonic() + 30
            while time.monotonic() < limit:
                st = proc_stat(helper)
                if st is None or st[1] != self.child:
                    gone = time.monotonic_ns()
                    end_off = len(self.raw)
                    break
                if st[0] == 'Z' and zombie_exit is None:
                    zombie_exit = st[2]
                self.pump(.001)
        self.pump(1.5 if not unarmed else 1.0)
        if unarmed:
            self.wait(lambda: (self.composer() or '').find(f'M59_EDITED_{i}') >= 0, 5)
        self.pump(.5)
        trace = (self.trace.read_text() if self.trace.exists() else '')[len(before_trace):]
        events = []
        for line in trace.splitlines():
            t, _, event = line.partition(' ')
            events.append({'t': int(t), 'event': event})
        branch = next((e for e in events if e['event'].startswith('branch ')), None)
        start = next((e for e in events if e['event'] == 'start'), None)
        status = None
        if zombie_exit is not None:
            status = {'exited': os.WIFEXITED(zombie_exit) if zombie_exit >= 0 else None,
                      'code': os.WEXITSTATUS(zombie_exit) if os.WIFEXITED(zombie_exit) else None,
                      'signal': os.WTERMSIG(zombie_exit) if os.WIFSIGNALED(zombie_exit) else None,
                      'raw': zombie_exit}
        editors = self.editor_entries()[editors_before:]
        window = bytes(self.raw[raw_before:])
        self.trials.append({
            'trial': i, 'draft': draft, 'editor_rc_planned': rc, 'sent_t': sent, 'helper_pid': helper,
            'helper_seen_t': seen, 'helper_gone_t': gone, 'zombie_status': status,
            'trace': events,
            'helper_life_from_start_s': (gone - start['t']) / 1e9 if gone and start else None,
            'helper_life_from_branch_s': (gone - branch['t']) / 1e9 if gone and branch else None,
            'helper_life_from_ctrl_g_s': (gone - sent) / 1e9 if gone else None,
            'editor': editors, 'notice_in_output': NOTICE in window,
            # Claude's editor notices, split at the moment the recorder saw
            # the helper gone: what Claude wrote from the Ctrl+G to then,
            # and what it wrote after. The test reads a notice that is only
            # in the second as Claude's report of this helper's end.
            'claude_notice_before_end': claude_notices(bytes(self.raw[raw_before:end_off])) if end_off is not None else None,
            'claude_notice_after_end': claude_notices(bytes(self.raw[end_off:])) if end_off is not None else None,
            'composer_after': self.composer(), 'screen_after': self.text()})

    def run(self):
        env = dict(os.environ)
        for key in ('COLORTERM',):
            env.pop(key, None)
        editor_cmd = ' '.join([sys.executable, '-I', self.editor_script, '--editor', str(self.out), 'user'])
        env.update({
            'TERM': 'xterm-256color', 'LANG': 'C.UTF-8', 'DISABLE_AUTOUPDATER': '1',
            'VISUAL': self.helper, 'EDITOR': self.helper,
            'DOT_AGENT_DECK_EDITOR_GEN': self.generation,
            'DOT_AGENT_DECK_EDITOR_USER_VISUAL': editor_cmd,
            'DOT_AGENT_DECK_EDITOR_USER_EDITOR': editor_cmd,
            'DOT_AGENT_DECK_SOCKET': self.socket,
            'DOT_AGENT_DECK_PANE_ID': 'm59', 'DOT_AGENT_DECK_TOKEN': 'm59',
            'DOT_AGENT_DECK_E2E_HELPER_TRACE': str(self.trace),
            'OTEL_SDK_DISABLED': 'true',
        })
        if self.cell == 'armed-stall':
            fifo = self.out / 'pause.fifo'
            os.mkfifo(fifo, 0o600)
            env['DOT_AGENT_DECK_E2E_HELPER_PAUSE'] = str(fifo)
            env['DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT'] = 'ready'
        self.child, self.fd = pty.fork()
        if self.child == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', 50, 200, 0, 0))
            os.execve(self.binary, [self.binary, '--model', 'haiku', '--setting-sources', 'user'], env)
        (self.out / 'claude.pid').write_text(str(self.child))
        try:
            if not self.wait(lambda: (self.out / 'identity.ok').exists(), 30):
                raise RuntimeError('the test wrote no identity for Claude')
            if not self.wait(lambda: self.composer() is not None, 45):
                raise RuntimeError('no composer: ' + self.text())
            self.pump(2)
            # The unarmed cell adds an eleventh press whose editor exits 37,
            # so ten zero-status edits stay ten.
            for i in range(1, TRIALS + 1 + (self.cell == 'unarmed-long')):
                self.trial(i)
        finally:
            os.kill(self.child, signal.SIGTERM)
            deadline = time.monotonic() + 3
            while os.waitpid(self.child, os.WNOHANG) == (0, 0):
                if time.monotonic() >= deadline:
                    os.kill(self.child, signal.SIGKILL)
                    os.waitpid(self.child, 0)
                    break
                time.sleep(.02)
            os.close(self.fd)
            (self.out / 'pty.bin').write_bytes(self.raw)
            (self.out / 'pty.json').write_text(json.dumps({'reads': self.reads, 'events': self.events}))


def main():
    global pyte
    if sys.argv[1] == '--block-hook':
        block_hook(Path(sys.argv[2]))
        return
    import pyte
    cell, out, helper, socket, generation, editor_script = sys.argv[1:7]
    out = Path(out)
    home = Path(os.environ['HOME']).resolve()
    assert home.name == 'home' and home.parent == Path.cwd().resolve(), 'requires TuiDeck private HOME'
    binary = Path(shutil.which('claude')).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    release = subprocess.check_output([str(binary), '--version'], text=True).split()[0]
    assert PINS.get(release) == digest, 'executable pin changed'
    out.mkdir(mode=0o700)
    settings = home / '.claude/settings.json'
    hook = ' '.join(shlex.quote(x) for x in [sys.executable, str(Path(__file__).resolve()), '--block-hook', str(out)])
    configuration = json.loads(settings.read_text()) if settings.exists() else {}
    configuration['autoUpdates'] = False
    # Only the blocking hook: the deck's own hooks would reach the test's
    # unanswering socket through the DOT_AGENT_DECK_SOCKET this run sets.
    configuration['hooks'] = {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': hook}]}]}
    settings.write_text(json.dumps(configuration))
    run = Run(cell, out, helper, socket, generation, editor_script)
    run.binary = str(binary)
    error = None
    try:
        run.run()
    except Exception as exc:
        error = repr(exc)
    finally:
        (out / 'result.json').write_text(json.dumps(
            {'cell': cell, 'release': release, 'sha256': digest, 'error': error, 'claude_pid': getattr(run, 'child', None),
             'trials': run.trials,
             'recorder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()},
            indent=1, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        if sys.argv[1] == '--block-hook':
            print('measurement hook failed closed', file=sys.stderr)
            sys.exit(2)
        (Path(sys.argv[2]).parent / 'launcher-error.json').write_text(json.dumps(str(exc)))
        raise
