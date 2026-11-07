#!/usr/bin/env python3
"""M59 on Codex: a real pinned Codex running the deck's editor helper
(PRD #1666 part 3, U4), the Codex twin of `claude_helper_alarm_measure.py`.

Invoked only by `tests/e2e_codex_helper_alarm.rs`. No credential: the
sandbox, dummy key and loopback Responses server of `codex_editor_measure.py`
(imported from beside this file), inside `bwrap --unshare-net` with /home,
/root, /tmp and /run/user empty. Codex's VISUAL and EDITOR name the helper
copy of an instance the test created; the helper's daemon socket is one the
test process owns and never answers. The user's editor, which the helper
restores, is `codex_editor_measure.py`'s stand-in. Nothing is submitted.

    codex_helper_alarm_measure.py <cell> <root> <codex> <helper> <socket> <generation>

`<root>` must not exist (under /var/tmp); `<root>/capture/codex.pid` names
Codex once it runs, and the recorder waits for `<root>/capture/identity.ok`
(the test writing Codex's identity into the generation) before it starts.

Cells:
  armed-stall   arm present; the helper pauses at its first ready poll (the
                e2e pause seam, on a FIFO nobody writes), so only its 5 s
                alarm can end it
  armed-timeout arm present; no pause: the unanswered exchange runs out
  unarmed-long  arm absent; the user's editor sleeps 8 s, writes a sentinel
                and exits 0, ten times, then once exiting 37
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

TRIALS = 10
NOTICE = b'dot-agent-deck: the deck was checking this input box.'


def proc_stat(pid):
    """(state, ppid, exit_code) from /proc/<pid>/stat, or None once reaped."""
    try:
        raw = Path(f'/proc/{pid}/stat').read_text()
    except OSError:
        return None
    fields = raw[raw.rindex(')') + 2:].split()
    return fields[0], int(fields[1]), int(fields[49]) if len(fields) > 49 else None


def exe_of(pid):
    try:
        return os.readlink(f'/proc/{pid}/exe')
    except OSError:
        return None


def outer(cell, root, codex, helper, socket, generation):
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import codex_editor_measure as cem
    root = Path(root)
    assert str(root).startswith('/var/tmp/'), 'the root must be under /var/tmp'
    native = Path(codex).resolve(strict=True)
    digest = hashlib.sha256(native.read_bytes()).hexdigest()
    release = next((r for r, pin in cem.PINS.items() if pin == digest), None)
    assert release, 'Codex pin changed: %s %s' % (native, digest)
    root.mkdir(mode=0o700)
    for sub in ('home', 'work'):
        (root / sub).mkdir(mode=0o700)
    shutil.copy(__file__, root / 'recorder.py')
    shutil.copy(cem.__file__, root / 'codex_editor_measure.py')
    package = native.parent.parent
    env = {'PATH': '%s:/usr/local/bin:/usr/bin:/bin' % (package / 'codex-path'), 'TERM': 'xterm-256color',
           'COLORTERM': 'truecolor', 'LANG': 'C.UTF-8', 'HOME': str(root / 'home')}
    cmd = ['/usr/bin/bwrap', '--dev-bind', '/', '/', '--tmpfs', '/home', '--tmpfs', '/root',
           '--tmpfs', '/tmp', '--tmpfs', '/run/user', '--die-with-parent', '--unshare-net',
           '--chdir', str(root / 'work'), sys.executable, '-I', str(root / 'recorder.py'), '--inner', cell,
           str(root), str(native), release, digest, ':'.join(cem.find_pyte()), helper, socket, generation]
    print(root, flush=True)
    sys.exit(subprocess.run(cmd, env=env).returncode)


class Run:
    def __init__(self, cem, cell, root, binary, helper, socket, generation):
        self.cem, self.cell, self.root = cem, cell, root
        self.binary, self.helper, self.socket, self.generation = binary, helper, socket, generation
        self.out = root / 'capture'
        self.trials = []
        self.trace = self.out / 'helper-trace.log'
        self.editor_log = self.out / 'editor.jsonl'

    def descendants(self, pid):
        kids, frontier = [], [pid]
        while frontier:
            parent = frontier.pop()
            for entry in os.listdir('/proc'):
                if entry.isdigit():
                    st = proc_stat(int(entry))
                    if st and st[1] == parent:
                        kids.append(int(entry))
                        frontier.append(int(entry))
        return kids

    def helpers(self):
        return [p for p in self.descendants(self.s.child) if exe_of(p) == self.helper]

    def editor_entries(self):
        if not self.editor_log.exists():
            return []
        return [json.loads(x) for x in self.editor_log.read_text().splitlines() if x.strip()]

    def trial(self, i):
        s = self.s
        s.reset()
        draft = f'M59_DRAFT_{i}'
        s.send(draft, 'draft')
        s.wait(lambda: s.holds(draft), 2)
        s.pump(.3)
        unarmed = self.cell == 'unarmed-long'
        rc = 37 if unarmed and i == TRIALS + 1 else 0
        (self.out / 'plan.json').write_text(json.dumps(
            {'action': 'write', 'text': f'M59_EDITED_{i}', 'sleep': 8, 'rc': rc}))
        editors_before = len(self.editor_entries())
        before_trace = self.trace.read_text() if self.trace.exists() else ''
        raw_before = len(s.raw)
        sent = time.monotonic_ns()
        s.send('\x07', 'ctrl-g')
        helper = None
        until = time.monotonic() + 5
        while helper is None and time.monotonic() < until:
            pids = self.helpers()
            helper = pids[0] if pids else None
            if helper is None:
                s.read_once(.001)
        seen = time.monotonic_ns()
        zombie_exit = gone = end_off = parent = None
        if helper is not None:
            parent = proc_stat(helper)[1] if proc_stat(helper) else None
            limit = time.monotonic() + 30
            while time.monotonic() < limit:
                st = proc_stat(helper)
                if st is None or st[1] != parent:
                    gone, end_off = time.monotonic_ns(), len(s.raw)
                    break
                if st[0] == 'Z' and zombie_exit is None:
                    zombie_exit = st[2]
                s.read_once(.001)
        s.pump(1.5)
        if unarmed:
            s.wait(lambda: f'M59_EDITED_{i}' in (s.composer() or ''), 5)
        s.pump(.5)
        trace = (self.trace.read_text() if self.trace.exists() else '')[len(before_trace):]
        events = []
        for line in trace.splitlines():
            t, _, event = line.partition(' ')
            events.append({'t': int(t), 'event': event})
        branch = next((e for e in events if e['event'].startswith('branch ')), None)
        start = next((e for e in events if e['event'] == 'start'), None)
        status = None
        if zombie_exit is not None:
            status = {'code': os.WEXITSTATUS(zombie_exit) if os.WIFEXITED(zombie_exit) else None,
                      'signal': os.WTERMSIG(zombie_exit) if os.WIFSIGNALED(zombie_exit) else None,
                      'raw': zombie_exit}
        window = bytes(s.raw[raw_before:])
        errors_before = [l.strip() for l in s.text().splitlines() if '\u25a0' in l]
        self.trials.append({
            'trial': i, 'draft': draft, 'editor_rc_planned': rc, 'sent_t': sent, 'helper_pid': helper,
            'helper_parent_is_codex': parent == s.child, 'helper_seen_t': seen, 'helper_gone_t': gone,
            'zombie_status': status, 'trace': events,
            'helper_life_from_start_s': (gone - start['t']) / 1e9 if gone and start else None,
            'helper_life_from_branch_s': (gone - branch['t']) / 1e9 if gone and branch else None,
            'helper_life_from_ctrl_g_s': (gone - sent) / 1e9 if gone else None,
            'editor': self.editor_entries()[editors_before:], 'notice_in_output': NOTICE in window,
            # Codex's own `■` line after this press, and whether it was
            # drawn only after the recorder saw the helper gone.
            'codex_error_line': errors_before[-1] if errors_before else None,
            'codex_error_after_end': (b'Failed to open editor' in bytes(s.raw[end_off:])
                                      if end_off is not None else None),
            'sent_off': raw_before, 'end_off': end_off, 'done_off': len(s.raw),
            'composer_after': s.composer(), 'screen_after': s.text()})

    def run(self):
        cem = self.cem
        # The recorder's own Recorder only for its seed, editor stand-in
        # command and fake server.
        rec = cem.Recorder(self.root, self.binary, None, None)
        rec.out = self.out
        rec.log = self.editor_log
        rec.seed()
        # Option P: the deck creates `editor` 0700 before a direct Codex
        # starts; Codex would make it 0775 under this umask.
        (rec.codex_home / 'editor').mkdir(mode=0o700)
        user = rec.editor_command('user')
        env = {'VISUAL': self.helper, 'EDITOR': self.helper,
               'DOT_AGENT_DECK_EDITOR_GEN': self.generation,
               'DOT_AGENT_DECK_EDITOR_USER_VISUAL': user, 'DOT_AGENT_DECK_EDITOR_USER_EDITOR': user,
               'DOT_AGENT_DECK_SOCKET': self.socket, 'DOT_AGENT_DECK_PANE_ID': 'm59',
               'DOT_AGENT_DECK_TOKEN': 'm59', 'DOT_AGENT_DECK_E2E_HELPER_TRACE': str(self.trace),
               'OTEL_SDK_DISABLED': 'true'}
        if self.cell == 'armed-stall':
            fifo = self.out / 'pause.fifo'
            os.mkfifo(fifo, 0o600)
            env['DOT_AGENT_DECK_E2E_HELPER_PAUSE'] = str(fifo)
            env['DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT'] = 'ready'
        self.s = s = cem.Session(rec, 'pty', env)
        (self.out / 'codex.pid').write_text(str(s.child))
        try:
            if not s.wait(lambda: (self.out / 'identity.ok').exists(), 30):
                raise RuntimeError('the test wrote no identity for Codex')
            if not s.wait(s.ready, 30):
                raise RuntimeError('no ready composer: ' + s.text())
            s.pump(cem.SETTLE)
            for i in range(1, TRIALS + 1 + (self.cell == 'unarmed-long')):
                self.trial(i)
        finally:
            s.close()


def inner(cell, root, binary, release, digest, pyte_path, helper, socket, generation):
    sys.path[:0] = [str(Path(__file__).resolve().parent)] + [p for p in pyte_path.split(':') if p]
    import pyte
    import codex_editor_measure as cem
    cem.pyte = pyte
    root = Path(root)
    assert Path(os.environ['HOME']) == root / 'home', 'requires the sandbox HOME'
    assert not any(k.startswith(('OPENAI_', 'ANTHROPIC_', 'CODEX_', 'CLAUDE_')) for k in os.environ)
    assert not any(Path('/home').iterdir()), 'the real /home is visible'
    run = Run(cem, cell, root, binary, helper, socket, generation)
    error = None
    try:
        run.run()
    except Exception as exc:
        import traceback
        error = repr(exc) + '\n' + traceback.format_exc()
    finally:
        assert hashlib.sha256(Path(binary).read_bytes()).hexdigest() == digest, 'executable pin changed'
        (root / 'capture/result.json').write_text(json.dumps(
            {'cell': cell, 'release': release, 'sha256': digest, 'error': error,
             'codex_pid': getattr(getattr(run, 's', None), 'child', None), 'trials': run.trials,
             'env_names': sorted(os.environ),
             'recorder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
             'editor_recorder_sha256': hashlib.sha256(Path(cem.__file__).read_bytes()).hexdigest()},
            indent=1, ensure_ascii=False))


if __name__ == '__main__':
    if sys.argv[1] == '--inner':
        inner(*sys.argv[2:11])
    else:
        outer(*sys.argv[1:7])
