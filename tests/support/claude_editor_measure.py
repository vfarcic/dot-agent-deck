#!/usr/bin/env python3
"""Claude Code external-editor recorder (PRD #1666 part 3, editor_probe facts).

Invoked only by the credentialed Rust TUI harness (`tests/e2e_claude_editor_measure.rs`).
HOME, cwd and deck endpoints belong to TuiDeck; nothing outside its
disposable HOME and work directory is written. Every Claude child is captured
at fork and reaped before the next launch. No prompt is submitted, and a
private UserPromptSubmit hook blocks any that is, so no model turn starts.

The "editor" is this same script run with `--editor`: it logs its argv, its
environment's VISUAL/EDITOR/TMPDIR and the exported file, then follows the
plan the recorder left for it (keep the file, or write a sentinel, after an
optional sleep, exiting with a chosen status).

Modes (argv[1]):
  editor          export shape, argument rule, editor selection and the
                  ?2004 writes around each invocation
  keymap-start    M49b: a Ctrl+G-unbinding keybindings.json present before
                  start, exercised at the first composer frame, five launches
  keymap-unchanged  M49b: the same file unchanged for ten minutes
  keymap-reload   M49b: a file created, then removed, during a session
  keymap-project  the same unbinding in the project's .claude/keybindings.json
  forms           M44e: candidate wire forms of Ctrl+G in several states,
                  whole and split, and bracketed-paste BEL
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
import stat
import struct
import subprocess
import sys
import termios
import time

PINS = {
    '2.1.310': 'b73676430af12f97316a798b8371f81d640f90a58bdcb8f3ce56a835bb47df56',
    '2.1.312': '4f4178ce41d83e23341ebfbaecdbcf8ea6bf7baea0690025abe49eeec3a5d01d',
    '2.1.317': 'eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0',
}
UNBIND = {'bindings': [{'context': 'Chat', 'bindings': {'ctrl+g': None, 'ctrl+e': 'chat:externalEditor'}}]}
FORMS = [('bel', '\x07'), ('esc-bel', '\x1b\x07')]
for _cp in (103, 71):
    FORMS.append((f'u-{_cp}-implicit', f'\x1b[{_cp};5u'))
    for _event in (1, 2, 3):
        FORMS.append((f'u-{_cp}-event{_event}', f'\x1b[{_cp};5:{_event}u'))
    FORMS.append((f'mok-{_cp}', f'\x1b[27;5;{_cp}~'))
MODE_RE = re.compile(rb'\x1b\[\?([0-9;]+)([hl])')


def editor(out):
    """The stand-in user editor: log, follow the plan, exit."""
    log = out / 'editor.jsonl'
    path = Path(sys.argv[-1])
    plan = json.loads((out / 'plan.json').read_text())
    try:
        st = path.stat()
        parent = path.parent.stat()
        before = path.read_bytes()
    except OSError as exc:
        st = parent = None
        before = repr(exc).encode()
    row = {'stage': 'start', 't': time.monotonic_ns(), 'pid': os.getpid(), 'ppid': os.getppid(),
           'argv': sys.argv, 'cwd': os.getcwd(), 'path': str(path),
           'parent': str(path.parent), 'parent_mode': oct(stat.S_IMODE(parent.st_mode)) if parent else None,
           'parent_uid': parent.st_uid if parent else None,
           'file_mode': oct(stat.S_IMODE(st.st_mode)) if st else None,
           'contents_hex': before.hex(), 'visual': os.environ.get('VISUAL'),
           'editor': os.environ.get('EDITOR'), 'tmpdir': os.environ.get('TMPDIR'),
           'isatty': [os.isatty(fd) for fd in (0, 1, 2)], 'plan': plan}
    with log.open('a') as f:
        f.write(json.dumps(row) + '\n')
    time.sleep(plan.get('sleep', 0))
    if plan.get('action') == 'write':
        path.write_text(plan['text'])
    with log.open('a') as f:
        f.write(json.dumps({'stage': 'exit', 't': time.monotonic_ns(), 'pid': os.getpid(),
                            'rc': plan.get('rc', 0)}) + '\n')
    sys.exit(plan.get('rc', 0))


def block_hook(out):
    payload = json.load(sys.stdin)
    with (out / 'hook.jsonl').open('a') as log:
        log.write(json.dumps({'t': time.monotonic_ns(), 'payload': payload}) + '\n')
    print(json.dumps({'decision': 'block', 'reason': 'editor measurement; no model turn'}))


class Session:
    """One Claude launch on a private PTY, its raw output kept with read times."""

    def __init__(self, rec, name, env, rows=50, cols=200, sources=True):
        self.rec, self.name = rec, name
        self.raw, self.reads, self.events = bytearray(), [], []
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.child, self.fd = pty.fork()
        if self.child == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
            for key in ('COLORTERM', 'VISUAL', 'EDITOR'):
                os.environ.pop(key, None)
            os.environ.update({'TERM': 'xterm-256color', 'LANG': 'C.UTF-8', 'DISABLE_AUTOUPDATER': '1'})
            os.environ.update(env)
            # `--setting-sources user` keeps the harness's project settings
            # out; the project-keymap cell also launches without it, as a
            # deck pane does.
            extra = ['--setting-sources', 'user'] if sources else []
            os.execv(rec.binary, [rec.binary, '--model', 'haiku'] + extra)
        self.started = time.monotonic_ns()

    def text(self):
        return '\n'.join(row.rstrip() for row in self.screen.display)

    def pump(self, duration):
        until = time.monotonic() + duration
        while time.monotonic() < until:
            if not select.select([self.fd], [], [], min(.01, max(0, until - time.monotonic())))[0]:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b''
            if not data:
                raise RuntimeError(f'{self.name}: Claude exited during measurement')
            self.reads.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'len': len(data)})
            self.raw.extend(data)
            self.stream.feed(data)

    def wait(self, predicate, seconds=10):
        until = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= until:
                return False
            self.pump(.01)
        return True

    def send(self, text, label=''):
        self.events.append({'t': time.monotonic_ns(), 'send': text, 'label': label, 'off': len(self.raw)})
        data = text.encode()
        assert os.write(self.fd, data) == len(data), 'single write was partial'

    def composer_ready(self):
        return any(row.startswith('❯') for row in self.screen.display)

    def composer(self):
        rows = self.screen.display
        prompt = next((i for i, row in enumerate(rows) if row.startswith('❯')), None)
        if prompt is None:
            return None
        end = next((i for i in range(prompt + 1, len(rows)) if rows[i].startswith('─')), len(rows))
        return '\n'.join(row[2:].rstrip() for row in rows[prompt:end])

    def reset(self):
        # One Escape dismisses a menu or overlay; a second on an empty
        # composer would open Rewind, so a dialog left open is closed on its
        # own. Then a non-empty draft, so Ctrl+C clears rather than warns.
        time.sleep(.3)
        self.send('\x1b', 'dismiss')
        self.pump(.15)
        if 'Esc to cancel' in self.text():
            self.send('\x1b', 'dismiss-dialog')
            self.pump(.15)
        self.send('reset', 'reset')
        self.pump(.15)
        self.send('\x03', 'clear')
        self.pump(.5)

    def offset_at(self, t):
        off = 0
        for read in self.reads:
            if read['t'] > t:
                break
            off = read['off'] + read['len']
        return off

    def close(self):
        os.kill(self.child, signal.SIGTERM)
        deadline = time.monotonic() + 3
        while os.waitpid(self.child, os.WNOHANG) == (0, 0):
            if time.monotonic() >= deadline:
                os.kill(self.child, signal.SIGKILL)
                os.waitpid(self.child, 0)
                break
            time.sleep(.02)
        os.close(self.fd)
        stem = self.rec.out / self.name
        (stem.with_suffix('.bin')).write_bytes(self.raw)
        (stem.with_suffix('.json')).write_text(json.dumps(
            {'reads': self.reads, 'events': self.events, 'pid': self.child}, indent=1))


class Recorder:
    def __init__(self, out, binary, release, sha256):
        self.out, self.binary, self.release, self.sha256 = out, binary, release, sha256
        self.rows, self.notes = [], []
        self.log = out / 'editor.jsonl'
        self.home = Path(os.environ['HOME']).resolve()

    def plan(self, **plan):
        (self.out / 'plan.json').write_text(json.dumps(plan))

    def entries(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if line.strip()]

    def editor_command(self, tag):
        return ' '.join([sys.executable, '-I', str(Path(__file__).resolve()), '--editor', str(self.out), tag])

    def launch(self, name, env=None, sources=True):
        s = Session(self, name, env or {}, sources=sources)
        if not s.wait(s.composer_ready, 45):
            s.close()
            raise RuntimeError(f'{name}: no composer: ' + s.text())
        return s

    def invoke(self, s, key, label, plan, timeout=3, expect=None, split=False, gap=0.0):
        """Send `key` and record whether, and how, the editor ran."""
        self.plan(**plan)
        n = len(self.entries())
        sent = time.monotonic_ns()
        sent_off = len(s.raw)
        if split and len(key) > 1:
            at = max(1, len(key) // 2)
            s.send(key[:at], label + '-prefix')
            s.pump(gap)
            s.send(key[at:], label + '-suffix')
        else:
            s.send(key, label)
        opened = s.wait(lambda: any(e['stage'] == 'start' for e in self.entries()[n:]), timeout)
        start = exit_ = None
        if opened:
            s.wait(lambda: any(e['stage'] == 'exit' for e in self.entries()[n:]), plan.get('sleep', 0) + 10)
            new = self.entries()[n:]
            start = next(e for e in new if e['stage'] == 'start')
            exit_ = next((e for e in new if e['stage'] == 'exit'), None)
        if expect is not None:
            s.wait(lambda: s.composer() is not None and expect in s.composer(), 5)
        s.pump(.4)
        done_off = len(s.raw)
        row = {'label': label, 'key_hex': key.encode().hex(), 'split': split, 'gap_s': gap,
               'sent_t': sent, 'sent_off': sent_off, 'opened': opened, 'start': start, 'exit': exit_,
               'composer_after': s.composer(), 'screen_after': s.text()}
        if opened:
            start_off = s.offset_at(start['t'])
            exit_off = s.offset_at(exit_['t']) if exit_ else done_off
            row.update(start_off=start_off, exit_off=exit_off, done_off=done_off,
                       modes_before=self.modes(s.raw[sent_off:start_off]),
                       modes_during=self.modes(s.raw[start_off:exit_off]),
                       modes_after=self.modes(s.raw[exit_off:done_off]))
        else:
            row.update(done_off=done_off, modes_window=self.modes(s.raw[sent_off:done_off]))
        return row

    @staticmethod
    def modes(data):
        return [m.group(0).decode() for m in MODE_RE.finditer(bytes(data))]

    def export_rule(self, row):
        """Claude's export: `<TMPDIR>/claude-<uid>/...`, the editor's last argument."""
        start = row['start']
        tmpdir = start['tmpdir'] or '/tmp'
        root = Path(tmpdir).resolve() / f'claude-{os.getuid()}'
        path = Path(start['path'])
        return {'last_argument': start['argv'][-1] == start['path'],
                'under_tmpdir_claude_uid': str(path.resolve()).startswith(str(root) + os.sep),
                'tmpdir': tmpdir, 'root': str(root), 'path': start['path']}

    # ---- modes -------------------------------------------------------

    def run_editor(self):
        cells = []
        # VISUAL wins, split on literal spaces; five trials writing a sentinel.
        s = self.launch('visual', {'VISUAL': self.editor_command('visual'), 'EDITOR': self.editor_command('editor-ignored')})
        try:
            for i in range(1, 6):
                s.reset()
                draft = f'M317_DRAFT_{i}'
                s.send(draft, 'draft')
                s.pump(.4)
                row = self.invoke(s, '\x07', f'visual-{i}', {'action': 'write', 'text': f'M317_EDITED_{i}'},
                                  expect=f'M317_EDITED_{i}')
                row.update(cell='visual', trial=i, draft=draft)
                if row['opened']:
                    row['export_rule'] = self.export_rule(row)
                    row['export_had_draft'] = bytes.fromhex(row['start']['contents_hex']).decode(errors='replace')
                cells.append(row)
            # A non-zero editor status: what Claude does with the file.
            s.reset()
            s.send('M317_DRAFT_RC', 'draft')
            s.pump(.4)
            row = self.invoke(s, '\x07', 'visual-rc37', {'action': 'write', 'text': 'M317_EDITED_RC37', 'rc': 37})
            row.update(cell='visual-rc37')
            cells.append(row)
        finally:
            s.close()
        # VISUAL empty: EDITOR.
        s = self.launch('editor-only', {'VISUAL': '', 'EDITOR': self.editor_command('editor')})
        try:
            for i in range(1, 3):
                s.reset()
                s.send(f'M317_EDRAFT_{i}', 'draft')
                s.pump(.4)
                row = self.invoke(s, '\x07', f'editor-{i}', {'action': 'write', 'text': f'M317_EEDITED_{i}'},
                                  expect=f'M317_EEDITED_{i}')
                row.update(cell='editor', trial=i)
                cells.append(row)
        finally:
            s.close()
        # Neither set: the first of code, vi, nano on PATH.
        stubs = self.out / 'stubs'
        for names, cell in ((('vi', 'nano'), 'path-vi-nano'), (('code', 'vi', 'nano'), 'path-code-vi-nano')):
            shutil.rmtree(stubs, ignore_errors=True)
            stubs.mkdir()
            for name in names:
                stub = stubs / name
                stub.write_text('#!/bin/sh\nexec ' + self.editor_command('path-' + name) + ' "$@"\n')
                stub.chmod(0o700)
            s = self.launch(cell, {'VISUAL': '', 'EDITOR': '', 'PATH': f'{stubs}:{os.environ["PATH"]}'})
            try:
                s.reset()
                s.send('M317_PDRAFT', 'draft')
                s.pump(.4)
                row = self.invoke(s, '\x07', cell, {'action': 'write', 'text': 'M317_PEDITED'}, expect='M317_PEDITED')
                row.update(cell=cell, stubs=list(names))
                cells.append(row)
            finally:
                s.close()
        self.rows = cells

    def keybindings(self):
        return self.home / '.claude/keybindings.json'

    def run_keymap_start(self):
        for trial in range(5):
            self.keybindings().parent.mkdir(exist_ok=True)
            self.keybindings().write_text(json.dumps(UNBIND))
            s = Session(self, f'keymap-start-{trial}', {'VISUAL': self.editor_command('keymap')})
            try:
                # No ready pause: act on the first composer frame drawn.
                if not s.wait(s.composer_ready, 45):
                    raise RuntimeError('no first composer: ' + s.text())
                first = time.monotonic_ns()
                s.send('M49B_DRAFT', 'first-frame-draft')
                g = self.invoke(s, '\x07', 'unbound-ctrlg', {'action': 'keep'}, timeout=1.5)
                e = self.invoke(s, '\x05', 'bound-ctrle', {'action': 'keep'}, timeout=3)
                self.rows.append({'cell': 'keymap-start', 'trial': trial, 'first_composer_t': first,
                                  'ms_from_spawn_to_first_composer': (first - s.started) / 1e6,
                                  'ctrlg': g, 'ctrle': e})
            finally:
                s.close()
        self.keybindings().unlink()

    def run_keymap_unchanged(self):
        self.keybindings().parent.mkdir(exist_ok=True)
        self.keybindings().write_text(json.dumps(UNBIND))
        st = self.keybindings().stat()
        s = self.launch('keymap-unchanged', {'VISUAL': self.editor_command('keymap')})
        try:
            s.send('M49B_DRAFT', 'draft')
            s.pump(.3)
            started = time.monotonic()
            for minute in range(1, 11):
                while time.monotonic() < started + 60 * minute:
                    s.pump(.5)
                g = self.invoke(s, '\x07', f'unchanged-minute-{minute}', {'action': 'keep'}, timeout=1.5)
                now = self.keybindings().stat()
                self.rows.append({'cell': 'keymap-unchanged', 'minute': minute,
                                  'elapsed_s': time.monotonic() - started,
                                  'stat_unchanged': (now.st_mtime_ns, now.st_ctime_ns, now.st_ino, now.st_size)
                                  == (st.st_mtime_ns, st.st_ctime_ns, st.st_ino, st.st_size),
                                  'ctrlg': g})
            e = self.invoke(s, '\x05', 'unchanged-positive-ctrle', {'action': 'keep'}, timeout=3)
            self.rows.append({'cell': 'keymap-unchanged-positive', 'ctrle': e})
        finally:
            s.close()
        self.keybindings().unlink()

    def run_keymap_reload(self):
        if self.keybindings().exists():
            self.keybindings().unlink()
        s = self.launch('keymap-reload', {'VISUAL': self.editor_command('keymap')})
        try:
            s.pump(2)
            s.send('M49B_DRAFT', 'draft')
            s.pump(.3)
            base = self.invoke(s, '\x07', 'baseline-ctrlg', {'action': 'keep'}, timeout=3)
            self.rows.append({'cell': 'keymap-reload-baseline', 'ctrlg': base})
            for change in ('created', 'removed'):
                if change == 'created':
                    self.keybindings().parent.mkdir(exist_ok=True)
                    self.keybindings().write_text(json.dumps(UNBIND))
                else:
                    self.keybindings().unlink()
                t0 = time.monotonic_ns()
                samples = []
                for _ in range(40):
                    g = self.invoke(s, '\x07', f'{change}-ctrlg', {'action': 'keep'}, timeout=.25)
                    e = self.invoke(s, '\x05', f'{change}-ctrle', {'action': 'keep'}, timeout=.25)
                    samples.append({'elapsed_s': (time.monotonic_ns() - t0) / 1e9,
                                    'ctrlg_opened': g['opened'], 'ctrle_opened': e['opened'],
                                    'ctrlg_sent_s': (g['sent_t'] - t0) / 1e9, 'ctrle_sent_s': (e['sent_t'] - t0) / 1e9})
                    flipped = (not g['opened'] and e['opened']) if change == 'created' else (g['opened'] and not e['opened'])
                    if flipped:
                        break
                self.rows.append({'cell': f'keymap-reload-{change}', 'samples': samples,
                                  'flipped': bool(samples) and flipped})
        finally:
            s.close()

    def run_keymap_project(self):
        project = Path.cwd() / '.claude'
        project.mkdir(exist_ok=True)
        target = project / 'keybindings.json'
        target.write_text(json.dumps(UNBIND))
        try:
            for sources, cell in ((True, 'keymap-project'), (False, 'keymap-project-all-sources')):
                s = self.launch(cell, {'VISUAL': self.editor_command('keymap')}, sources=sources)
                try:
                    s.pump(1)
                    s.send('M39_DRAFT', 'draft')
                    s.pump(.3)
                    g = self.invoke(s, '\x07', 'project-ctrlg', {'action': 'keep'}, timeout=3)
                    e = self.invoke(s, '\x05', 'project-ctrle', {'action': 'keep'}, timeout=1.5)
                    self.rows.append({'cell': cell, 'setting_sources_user': sources,
                                      'project_file': str(target), 'ctrlg': g, 'ctrle': e})
                finally:
                    s.close()
        finally:
            target.unlink()

    def run_forms(self):
        s = self.launch('forms', {'VISUAL': self.editor_command('forms')})
        try:
            s.pump(2)
            states = ['composer', 'slash', 'help', 'ctrlr', 'bang', 'esc-esc']
            for state in states:
                for name, seq in FORMS:
                    for split in (False, True):
                        s.reset()
                        if state == 'slash':
                            s.send('/', 'slash-menu')
                        elif state == 'help':
                            s.send('?', 'help')
                        elif state == 'bang':
                            s.send('!MEASURE_DRAFT', 'shell-mode')
                        else:
                            s.send('MEASURE_DRAFT', 'draft')
                            s.pump(.1)
                            if state == 'ctrlr':
                                s.send('\x12', 'reverse-search')
                            elif state == 'esc-esc':
                                s.send('\x1b', 'escape-one')
                                s.pump(.04)
                                s.send('\x1b', 'escape-two')
                        s.pump(.3)
                        before = s.text()
                        row = self.invoke(s, seq, f'{state}-{name}', {'action': 'keep'}, timeout=1.2, split=split)
                        row.update(cell='forms', state=state, form=name, split=split, screen_before=before)
                        self.rows.append(row)
            for gap in (0, .03, .06):
                s.reset()
                s.send('MEASURE_DRAFT', 'draft')
                s.pump(.1)
                s.send('\x1b', 'esc-bel-gap')
                s.pump(gap)
                row = self.invoke(s, '\x07', 'esc-bel-gap', {'action': 'keep'}, timeout=1.2)
                row.update(cell='forms', state='composer', form='esc-bel-gap', gap_s=gap)
                self.rows.append(row)
            # P-PASTE: bracketed BEL, whole and split; the next export shows
            # whether the BEL stayed in the draft as text.
            for split in (False, True):
                s.reset()
                s.send('BASE', 'draft')
                s.pump(.2)
                row = self.invoke(s, '\x1b[200~PASTE\x07TAIL\x1b[201~', 'paste-bel', {'action': 'keep'},
                                  timeout=1.2, split=split)
                after = self.invoke(s, '\x07', 'paste-bel-export', {'action': 'keep'}, timeout=3)
                row.update(cell='paste-bel', split=split,
                           next_export_hex=after['start']['contents_hex'] if after['opened'] else None)
                self.rows.append(row)
        finally:
            s.close()

    def save(self, error):
        result = {'release': self.release, 'sha256': self.sha256, 'executable': self.binary, 'uid': os.getuid(),
                  'mode': self.mode, 'error': error, 'rows': self.rows, 'notes': self.notes,
                  'recorder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        (self.out / 'result.json').write_text(json.dumps(result, indent=1, ensure_ascii=False))


def main():
    global pyte
    if sys.argv[1] == '--editor':
        editor(Path(sys.argv[2]))
        return
    out = Path(sys.argv[2])
    if sys.argv[1] == '--block-hook':
        block_hook(out)
        return
    import pyte
    mode = sys.argv[1]
    home = Path(os.environ['HOME']).resolve()
    assert home.name == 'home' and home.parent == Path.cwd().resolve(), 'requires TuiDeck private HOME'
    assert Path(os.environ['DOT_AGENT_DECK_ATTACH_SOCKET']).parent == home.parent, 'requires private endpoint'
    binary = Path(shutil.which('claude')).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest in PINS.values(), 'executable pin changed'
    release = subprocess.check_output([str(binary), '--version'], text=True).split()[0]
    assert PINS[release] == digest, 'executable pin changed'
    out.mkdir(mode=0o700)
    settings = home / '.claude/settings.json'
    hook = ' '.join(shlex.quote(x) for x in [sys.executable, str(Path(__file__).resolve()), '--block-hook', str(out)])
    configuration = json.loads(settings.read_text()) if settings.exists() else {}
    configuration['autoUpdates'] = False
    configuration['hooks'] = {'UserPromptSubmit': [{'hooks': [{'type': 'command', 'command': hook}]}]}
    settings.write_text(json.dumps(configuration))
    recorder = Recorder(out, str(binary), release, digest)
    recorder.mode = mode
    error = None
    try:
        {'editor': recorder.run_editor, 'keymap-start': recorder.run_keymap_start,
         'keymap-unchanged': recorder.run_keymap_unchanged, 'keymap-reload': recorder.run_keymap_reload,
         'keymap-project': recorder.run_keymap_project, 'forms': recorder.run_forms}[mode]()
    except Exception as exc:
        error = repr(exc)
    finally:
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest, 'executable pin changed'
        recorder.save(error)


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        if sys.argv[1] in ('--block-hook', '--editor'):
            print('measurement helper failed closed: ' + repr(exc), file=sys.stderr)
            sys.exit(2)
        (Path(sys.argv[2]).parent / 'launcher-error.json').write_text(json.dumps(str(exc)))
        raise
