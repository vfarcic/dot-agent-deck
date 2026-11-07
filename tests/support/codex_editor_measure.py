#!/usr/bin/env python3
"""Codex external-editor recorder (PRD #1666 part 3, U4: the `editor_probe`
facts Codex's record would need, M58r and the E1F premise).

Credential-free by construction (the nollm setup, `spike/nollm/` on
`agent/dispatch-nollm`): a dummy API key in a fresh CODEX_HOME and a
`model_providers` entry pointing at a canned Responses server on 127.0.0.1,
inside `bwrap --unshare-net` with /home, /root, /tmp and /run/user replaced by
empty tmpfs mounts and the environment started from five names. No real home,
no agent config directory and no network but loopback is visible to Codex. No
prompt is submitted; nothing here needs a model reply.

Run as `codex_editor_measure.py <mode> <root>`: it makes `<root>` (which must
not exist, under /var/tmp), copies itself in, and re-runs itself there under
bwrap. `<root>/capture/result.json` holds every row; each launch's raw PTY
bytes are beside it (`<launch>.bin`, read times in `<launch>.json`).

The "editor" is this script run with `--editor`: it logs its argv, its
environment's VISUAL/EDITOR, the exported file and its directory, then follows
the plan the recorder left (keep the file or write a sentinel, after an
optional sleep, exiting with a chosen status).

Modes:
  editor   export shape, argument rule, editor selection, the ?2004 writes
           around each invocation, a failing editor and the missing-editor
           error line
  startup  when Ctrl+G starts to open the editor after Codex's first ready
           composer, ten launches
  config-writes  what Codex writes to its own config.toml as it starts,
           seven launches on one CODEX_HOME
  e1f      the E1F premise: LATE written at E1's ?2004h, at the first ?2026h
           after it, and while the editor runs, 20 trials each, and whether
           the next export holds it
  keymap   M58r: Codex's own behaviour for each key-binding source case (the
           deck's resolver reads the same files in the Rust test)
  forms    M44e: candidate wire forms of Ctrl+G in several states, whole and
           split, and a bracketed-paste BEL
"""
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
import threading
import time
import uuid

PINS = {'0.164.0': 'b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306'}
DEFAULT_CODEX = '/var/tmp/dad-cxhe-codex-0.164.0/x64/package/vendor/x86_64-unknown-linux-musl/bin/codex'
ROWS, COLS = 50, 200
DUMMY = 'sk-nollm-dummy-' + '0' * 32
# The deck's wrapper prepends this pair to every Codex it starts (#2002).
WRAP_PAIR = ['-c', 'shell_environment_policy.set={}']
BASE_ARGS = ['--sandbox', 'workspace-write', '--ask-for-approval', 'never'] + WRAP_PAIR
F6 = '\x1b[17~'
FORMS = [('bel', '\x07'), ('esc-bel', '\x1b\x07')]
for _cp in (103, 71):
    FORMS.append((f'u-{_cp}-implicit', f'\x1b[{_cp};5u'))
    for _event in (1, 2, 3):
        FORMS.append((f'u-{_cp}-event{_event}', f'\x1b[{_cp};5:{_event}u'))
    FORMS.append((f'mok-{_cp}', f'\x1b[27;5;{_cp}~'))
MARK_RE = re.compile(rb'\x1b\[\?(2004|2026)([hl])')
PLACEHOLDERS = ('Ask Codex to do anything', 'Ask a follow-up question')
TICK = .005
# Seconds after the first ready composer before a launch is used.
SETTLE = 4.0


# ---------------------------------------------------------------- outer ----

def find_pyte():
    """A directory to put on sys.path so this interpreter imports pyte (and
    wcwidth): DAD_PYTE_PATH, else a pyte built for this Python minor in the
    Nix store."""
    found = []
    if os.environ.get('DAD_PYTE_PATH'):
        found = os.environ['DAD_PYTE_PATH'].split(':')
    else:
        minor = 'python%d.%d' % sys.version_info[:2]
        for name in ('pyte', 'wcwidth'):
            hits = sorted(Path('/nix/store').glob(f'*-{minor}-{name}-*/lib/{minor}/site-packages'))
            if hits:
                found.append(str(hits[-1]))
    return found


def outer(mode, root, codex):
    root = Path(root)
    assert str(root).startswith('/var/tmp/'), 'the root must be under /var/tmp'
    native = Path(codex).resolve(strict=True)
    digest = hashlib.sha256(native.read_bytes()).hexdigest()
    release = next((r for r, pin in PINS.items() if pin == digest), None)
    assert release, 'Codex pin changed: %s %s' % (native, digest)
    pyte_path = find_pyte()
    root.mkdir(mode=0o700)
    for sub in ('home', 'work'):
        (root / sub).mkdir(mode=0o700)
    shutil.copy(__file__, root / 'recorder.py')
    package = native.parent.parent
    path = '%s:/usr/local/bin:/usr/bin:/bin' % (package / 'codex-path')
    env = {'PATH': path, 'TERM': 'xterm-256color', 'COLORTERM': 'truecolor', 'LANG': 'C.UTF-8',
           'HOME': str(root / 'home')}
    cmd = ['/usr/bin/bwrap', '--dev-bind', '/', '/', '--tmpfs', '/home', '--tmpfs', '/root',
           '--tmpfs', '/tmp', '--tmpfs', '/run/user', '--die-with-parent', '--unshare-net',
           '--chdir', str(root / 'work'), sys.executable, '-I', str(root / 'recorder.py'), '--inner', mode,
           str(root), str(native), release, digest, ':'.join(pyte_path)]
    print(root, flush=True)
    proc = subprocess.run(cmd, env=env)
    sys.exit(proc.returncode)


# ----------------------------------------------------------- fake model ----

def serve_responses(log_path):
    """A canned OpenAI Responses server on 127.0.0.1 (nollm's, Responses
    only). Codex never reaches it here unless a line is submitted; it is
    there so the dummy key's provider answers rather than retries."""
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    class H(BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *a):
            pass

        def _log(self):
            with open(log_path, 'a') as f:
                f.write(json.dumps({'t': time.time(), 'method': self.command, 'path': self.path}) + '\n')

        def _json(self, code, obj):
            data = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            self._log()
            if self.path.split('?')[0].endswith('/models'):
                return self._json(200, {'object': 'list', 'data': [], 'models': []})
            self._json(404, {'error': {'type': 'not_found'}})

        def do_POST(self):
            n = int(self.headers.get('Content-Length') or 0)
            body = self.rfile.read(n) if n else b''
            self._log()
            if not self.path.split('?')[0].endswith('/responses'):
                return self._json(404, {'error': {'type': 'not_found'}})
            try:
                model = json.loads(body or b'{}').get('model', 'fake')
            except ValueError:
                model = 'fake'
            rid, iid = 'resp_' + uuid.uuid4().hex[:24], 'msg_' + uuid.uuid4().hex[:24]
            item = {'type': 'message', 'id': iid, 'role': 'assistant', 'status': 'completed',
                    'content': [{'type': 'output_text', 'text': 'NOLLM-CANNED-REPLY', 'annotations': []}]}
            base = {'id': rid, 'object': 'response', 'created_at': int(time.time()), 'model': model, 'output': []}
            events = [('response.created', {'type': 'response.created', 'response': dict(base, status='in_progress')}),
                      ('response.completed', {'type': 'response.completed',
                                              'response': dict(base, status='completed', output=[item])})]
            self.close_connection = True
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            for name, data in events:
                self.wfile.write(b'event: ' + name.encode() + b'\ndata: ' + json.dumps(data).encode() + b'\n\n')
            self.wfile.flush()

    httpd = ThreadingHTTPServer(('127.0.0.1', 0), H)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


# --------------------------------------------------------------- editor ----

def editor(out):
    """The stand-in user editor: log, follow the plan, exit."""
    log = out / 'editor.jsonl'
    path = Path(sys.argv[-1])
    plan = json.loads((out / 'plan.json').read_text())
    try:
        st, parent, before = path.stat(), path.parent.stat(), path.read_bytes()
    except OSError as exc:
        st = parent = None
        before = repr(exc).encode()
    row = {'stage': 'start', 't': time.monotonic_ns(), 'pid': os.getpid(), 'ppid': os.getppid(),
           'argv': sys.argv, 'path': str(path), 'parent': str(path.parent),
           'parent_mode': oct(stat.S_IMODE(parent.st_mode)) if parent else None,
           'parent_uid': parent.st_uid if parent else None,
           'file_mode': oct(stat.S_IMODE(st.st_mode)) if st else None,
           'contents_hex': before.hex(), 'visual': os.environ.get('VISUAL'),
           'editor': os.environ.get('EDITOR'), 'isatty': [os.isatty(fd) for fd in (0, 1, 2)], 'plan': plan}
    with log.open('a') as f:
        f.write(json.dumps(row) + '\n')
    time.sleep(plan.get('sleep', 0))
    if plan.get('action') == 'write':
        path.write_text(plan['text'])
    with log.open('a') as f:
        f.write(json.dumps({'stage': 'exit', 't': time.monotonic_ns(), 'pid': os.getpid(),
                            'rc': plan.get('rc', 0)}) + '\n')
    sys.exit(plan.get('rc', 0))


# -------------------------------------------------------------- session ----

class Exited(RuntimeError):
    pass


class Session:
    """One Codex launch on a private PTY, its raw output kept with read times."""

    def __init__(self, rec, name, env=None, args=(), cwd=None):
        import fcntl
        import pty
        import termios
        self.rec, self.name = rec, name
        self.raw, self.reads, self.events = bytearray(), [], []
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.argv = [rec.binary] + BASE_ARGS + list(args)
        self.exit_status = None
        self.child, self.fd = pty.fork()
        if self.child == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack('HHHH', ROWS, COLS, 0, 0))
            for key in ('VISUAL', 'EDITOR'):
                os.environ.pop(key, None)
            os.environ.update(env or {})
            if cwd:
                os.chdir(cwd)
            os.execv(rec.binary, self.argv)
        self.started = time.monotonic_ns()

    def text(self):
        return '\n'.join(row.rstrip() for row in self.screen.display)

    def read_once(self, timeout):
        if not select.select([self.fd], [], [], max(0, timeout))[0]:
            return False
        try:
            data = os.read(self.fd, 65536)
        except OSError:
            data = b''
        if not data:
            raise Exited(f'{self.name}: Codex exited: ' + self.text()[-600:])
        self.reads.append({'t': time.monotonic_ns(), 'off': len(self.raw), 'len': len(data)})
        self.raw.extend(data)
        self.stream.feed(data)
        return True

    def pump(self, duration):
        until = time.monotonic() + duration
        while time.monotonic() < until:
            self.read_once(min(.01, max(0, until - time.monotonic())))

    def wait(self, predicate, seconds=10):
        until = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= until:
                return False
            self.read_once(.01)
        return True

    def send(self, text, label=''):
        self.events.append({'t': time.monotonic_ns(), 'send': text, 'label': label, 'off': len(self.raw)})
        data = text.encode()
        assert os.write(self.fd, data) == len(data), 'single write was partial'

    def glyph(self, row):
        cell = self.screen.buffer[row][0]
        return cell.data == '›' and not cell.reverse

    def numbered(self, row):
        return re.match(r'\d+\.', self.screen.display[row][2:]) is not None

    def composer(self):
        rows = self.screen.display
        prompt = next((r for r in range(ROWS - 1, -1, -1) if self.glyph(r) and not self.numbered(r)), None)
        if prompt is None:
            return None
        text = rows[prompt][2:].rstrip()
        for r in range(prompt + 1, ROWS):
            nxt = rows[r][2:].strip()
            if not nxt or not rows[r].startswith('  '):
                break
            text += ' ' + nxt
        return text

    def ready(self):
        c = self.composer()
        text = self.text()
        return (c is not None and c in PLACEHOLDERS and 'for shortcuts' in text
                and 'enter continue' not in text and 'Trust this folder' not in text)

    def holds(self, text):
        return self.composer() == text

    def reset(self):
        """Close any popup and empty the composer without Ctrl+C (which, on
        an empty composer, arms Codex's quit). The Escape goes to a
        composer holding text (the `esc-empty` cell shows an Escape on an
        empty one changes nothing for the next Ctrl+G)."""
        self.pump(.2)
        self.send('x', 'reset-text')
        self.pump(.1)
        self.send('\x1b', 'dismiss')
        self.pump(.15)
        for _ in range(3):
            self.send('\x1b[F\x15', 'clear')
            self.pump(.05)
        self.wait(lambda: self.composer() in PLACEHOLDERS, 2)

    def offset_at(self, t):
        off = 0
        for read in self.reads:
            if read['t'] > t:
                break
            off = read['off'] + read['len']
        return off

    def close(self):
        try:
            os.kill(self.child, signal.SIGTERM)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + 3
        while True:
            pid, status = os.waitpid(self.child, os.WNOHANG)
            if pid:
                self.exit_status = status
                break
            if time.monotonic() >= deadline:
                os.kill(self.child, signal.SIGKILL)
                self.exit_status = os.waitpid(self.child, 0)[1]
                break
            time.sleep(.02)
        os.close(self.fd)
        stem = self.rec.out / self.name
        stem.with_suffix('.bin').write_bytes(self.raw)
        stem.with_suffix('.json').write_text(json.dumps(
            {'reads': self.reads, 'events': self.events, 'pid': self.child, 'argv': self.argv}, indent=1))


def marks(data, base=0):
    """Every exactly-spelled ?2004l/?2004h/?2026h/?2026l, with its offset."""
    return [{'off': base + m.start(), 'mark': m.group(0)[2:].decode()} for m in MARK_RE.finditer(bytes(data))]


# ------------------------------------------------------------- recorder ----

class Recorder:
    def __init__(self, root, binary, release, sha256):
        self.root, self.binary, self.release, self.sha256 = root, binary, release, sha256
        self.out = root / 'capture'
        self.out.mkdir(mode=0o700)
        self.home = root / 'home'
        self.work = root / 'work'
        self.codex_home = self.home / '.codex'
        self.rows, self.notes = [], []
        self.log = self.out / 'editor.jsonl'
        self.server = serve_responses(self.out / 'model-requests.jsonl')
        self.port = self.server.server_address[1]

    def base_config(self, extra=''):
        return '\n'.join([
            'model = "nollm-model"', 'model_provider = "nollm"', 'check_for_update_on_startup = false', '',
            f'[projects."{self.work}"]', 'trust_level = "trusted"', '',
            '[model_providers.nollm]', 'name = "nollm"', f'base_url = "http://127.0.0.1:{self.port}/v1"',
            'wire_api = "responses"', 'requires_openai_auth = true', 'request_max_retries = 0',
            'stream_max_retries = 0', '', extra])

    def seed(self, extra=''):
        """A fresh CODEX_HOME: the dummy key, the fake provider and the work
        directory trusted; `extra` appended to config.toml."""
        if self.codex_home.exists():
            shutil.rmtree(self.codex_home)
        project = self.work / '.codex'
        if project.exists():
            shutil.rmtree(project)
        self.codex_home.mkdir(mode=0o700)
        (self.codex_home / 'auth.json').write_text(json.dumps({'auth_mode': 'apikey', 'OPENAI_API_KEY': DUMMY}))
        os.chmod(self.codex_home / 'auth.json', 0o600)
        (self.codex_home / 'config.toml').write_text(self.base_config(extra))

    def plan(self, **plan):
        (self.out / 'plan.json').write_text(json.dumps(plan))

    def entries(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines() if line.strip()]

    def editor_words(self, tag):
        return [sys.executable, '-I', str(Path(__file__).resolve()), '--editor', str(self.out), tag]

    def editor_command(self, tag):
        """The editor as a shell-words command line, with the tag quoted so
        that a literal-space split and a shell-words split differ."""
        import shlex
        return ' '.join(shlex.quote(w) for w in self.editor_words(tag))

    def launch(self, name, env=None, args=(), wait=30, settle=SETTLE):
        """A launch at its ready composer, then `settle` seconds more: a
        Ctrl+G about a second after start opens nothing (the `startup`
        mode measures when it starts to)."""
        s = Session(self, name, env, args)
        try:
            ok = s.wait(s.ready, wait)
            if ok:
                s.pump(settle)
        except Exited:
            s.close()
            raise
        if not ok:
            s.close()
            raise RuntimeError(f'{name}: no ready composer: ' + s.text())
        return s

    def invoke(self, s, key, label, plan, timeout=3, expect=None, split=False, gap=0.0):
        """Send `key` and record whether, and how, the editor ran, with the
        mode writes before, during and after it."""
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
        row = {'label': label, 'key_hex': key.encode().hex(), 'split': split, 'gap_s': gap, 'launch': s.name,
               'sent_t': sent, 'sent_off': sent_off, 'opened': opened, 'start': start, 'exit': exit_,
               'composer_after': s.composer(), 'screen_after': s.text(), 'done_off': done_off,
               'marks': marks(s.raw[sent_off:done_off], sent_off)}
        if opened:
            row['start_off'] = s.offset_at(start['t'])
            row['exit_off'] = s.offset_at(exit_['t']) if exit_ else done_off
        return row

    def export_rule(self, row):
        """Codex's export: `<CODEX_HOME>/editor/.tmpXXXXXX.md`, the editor's
        last argument."""
        start = row['start']
        path = Path(start['path'])
        return {'last_argument': start['argv'][-1] == start['path'],
                'in_codex_editor_dir': path.parent == self.codex_home / 'editor',
                'name_shape': re.fullmatch(r'\.tmp[A-Za-z0-9]{6}\.md', path.name) is not None,
                'parent_mode': start['parent_mode'], 'path': start['path']}

    # ---- editor ----------------------------------------------------------

    def run_editor(self):
        self.seed()
        cells = []
        # VISUAL wins over EDITOR, split by shell words; five trials writing
        # a sentinel, then a failing editor.
        s = self.launch('visual', {'VISUAL': self.editor_command('visual tag'),
                                   'EDITOR': self.editor_command('editor-ignored')})
        try:
            for i in range(1, 6):
                s.reset()
                draft = f'CXM_DRAFT_{i}'
                s.send(draft, 'draft')
                s.wait(lambda: s.holds(draft), 2)
                row = self.invoke(s, '\x07', f'visual-{i}', {'action': 'write', 'text': f'CXM_EDITED_{i}'},
                                  expect=f'CXM_EDITED_{i}')
                row.update(cell='visual', trial=i, draft=draft)
                if row['opened']:
                    row['export_rule'] = self.export_rule(row)
                    row['export_had_draft'] = bytes.fromhex(row['start']['contents_hex']).decode(errors='replace')
                cells.append(row)
            s.reset()
            s.send('CXM_DRAFT_RC', 'draft')
            s.wait(lambda: s.holds('CXM_DRAFT_RC'), 2)
            row = self.invoke(s, '\x07', 'visual-rc37', {'action': 'write', 'text': 'CXM_EDITED_RC37', 'rc': 37})
            row.update(cell='visual-rc37')
            cells.append(row)
            # An Escape on an empty composer, then a draft and Ctrl+G; the
            # control is the same with no Escape.
            for i in range(1, 4):
                for esc in (True, False):
                    s.reset()
                    s.pump(.3)
                    if esc:
                        s.send('\x1b', 'esc-empty')
                        s.pump(.3)
                    draft = f'CXM_ESC_{int(esc)}_{i}'
                    s.send(draft, 'draft')
                    s.wait(lambda: s.holds(draft), 2)
                    before = s.text()
                    row = self.invoke(s, '\x07', f'esc-empty-{int(esc)}-{i}', {'action': 'keep'}, timeout=2)
                    row.update(cell='esc-empty' if esc else 'esc-empty-control', trial=i, screen_before=before)
                    cells.append(row)
        finally:
            s.close()
        # The other selections, one launch each.
        stubs = self.out / 'stubs'
        stubs.mkdir()
        for name in ('vi', 'nano', 'code'):
            stub = stubs / name
            stub.write_text('#!/bin/sh\nexec ' + self.editor_command('path-' + name) + ' "$@"\n')
            stub.chmod(0o700)
        path = f'{stubs}:{os.environ["PATH"]}'
        cases = [
            ('editor-only', {'EDITOR': self.editor_command('editor')}),
            ('visual-empty-editor-set', {'VISUAL': '', 'EDITOR': self.editor_command('editor')}),
            ('editor-empty', {'EDITOR': ''}),
            ('neither-path-stubs', {'PATH': path}),
        ]
        for cell, env in cases:
            s = self.launch(cell, env)
            try:
                for i in range(1, 3):
                    s.reset()
                    draft = f'CXM_{cell.upper().replace("-", "_")}_{i}'
                    s.send(draft, 'draft')
                    s.wait(lambda: s.holds(draft), 2)
                    row = self.invoke(s, '\x07', f'{cell}-{i}', {'action': 'write', 'text': f'CXM_SEL_EDITED_{i}'},
                                      expect=None, timeout=3)
                    row.update(cell=cell, trial=i, env={k: v for k, v in env.items() if k != 'PATH'},
                               path_stubs=cell.endswith('stubs'), draft=draft)
                    cells.append(row)
            finally:
                s.close()
        self.rows = cells

    # ---- start-up ---------------------------------------------------------

    def run_startup(self, launches=10):
        """When Ctrl+G starts to open the editor: at the first ready
        composer, a draft and Ctrl+G, then Ctrl+G again every 250 ms until
        the editor runs (at most 8 s)."""
        self.seed()
        for n in range(1, launches + 1):
            s = Session(self, f'startup-{n}', {'VISUAL': self.editor_command('startup')})
            tries = []
            try:
                if not s.wait(s.ready, 30):
                    raise RuntimeError('no ready composer: ' + s.text())
                ready_t = time.monotonic_ns()
                first_t = s.reads[0]['t']
                s.send('CXM_START', 'draft')
                s.wait(lambda: s.holds('CXM_START'), 2)
                opened = False
                while not opened and time.monotonic_ns() - ready_t < 8e9:
                    row = self.invoke(s, '\x07', 'startup-ctrl-g', {'action': 'keep'}, timeout=.25)
                    error = [l.strip() for l in row['screen_after'].splitlines() if '\u25a0' in l]
                    tries.append({'sent_after_ready_s': (row['sent_t'] - ready_t) / 1e9,
                                  'sent_after_first_output_s': (row['sent_t'] - first_t) / 1e9,
                                  'opened': row['opened'], 'error_line': error[-1] if error else None})
                    opened = row['opened']
                self.rows.append({'cell': 'startup', 'launch': n,
                                  'ready_after_first_output_s': (ready_t - first_t) / 1e9, 'tries': tries})
            finally:
                s.close()

    # ---- config writes -------------------------------------------------------

    def run_config_writes(self, launches=7):
        """What Codex writes to its own `config.toml` as it starts: seven
        launches on one CODEX_HOME, each read until 8 s after its first
        ready composer, with the file's contents and mtime before the
        spawn, at the first ready composer, and at the end."""
        import difflib
        self.seed()
        config = self.codex_home / 'config.toml'

        def read():
            st = config.stat()
            return config.read_text(), st.st_mtime_ns

        for n in range(1, launches + 1):
            before, before_mtime = read()
            s = Session(self, f'config-writes-{n}', {'VISUAL': self.editor_command('config')})
            try:
                if not s.wait(s.ready, 30):
                    raise RuntimeError('no ready composer: ' + s.text())
                ready_wall = time.time_ns()
                at_ready, at_ready_mtime = read()
                s.pump(8)
                after, after_mtime = read()
            finally:
                s.close()
            self.rows.append({
                'cell': 'config-writes', 'launch': n,
                'changed_by_ready': at_ready != before, 'changed_after_ready': after != at_ready,
                'mtime_after_ready_ms': (after_mtime - ready_wall) / 1e6 if after_mtime != before_mtime else None,
                'diff': ''.join(difflib.unified_diff(before.splitlines(True), after.splitlines(True),
                                                     'before', 'after', n=0))})

    # ---- E1F --------------------------------------------------------------

    def run_e1f(self, trials=20):
        self.seed()
        for arm in ('at-2004h', 'at-2026h', 'during-editor'):
            s = self.launch(f'e1f-{arm}', {'VISUAL': self.editor_command('e1f')})
            try:
                for i in range(1, trials + 1):
                    s.reset()
                    s.send('BASE', 'draft')
                    s.wait(lambda: s.holds('BASE'), 2)
                    s.pump(.2)
                    row = self.e1f_trial(s, arm, i)
                    row['launch'] = s.name
                    self.rows.append(row)
            finally:
                s.close()

    def e1f_trial(self, s, arm, trial):
        plan = {'action': 'keep', 'sleep': .3 if arm == 'during-editor' else 0}
        self.plan(**plan)
        n = len(self.entries())
        sent_off = len(s.raw)
        s.send('\x07', f'e1f-{arm}-trigger')
        sent = time.monotonic_ns()
        late_off = late_t = None
        disable_off = enable_off = frame_off = None
        deadline = time.monotonic() + 5
        while late_off is None and time.monotonic() < deadline:
            if arm == 'during-editor':
                if any(e['stage'] == 'start' for e in self.entries()[n:]):
                    late_t, late_off = time.monotonic_ns(), len(s.raw)
                    s.send('LATE', f'e1f-{arm}-late')
                    break
                s.read_once(TICK)
                continue
            if not s.read_once(TICK):
                continue
            found = marks(s.raw[sent_off:], sent_off)
            disable = next((m for m in found if m['mark'] == '?2004l'), None)
            if disable is None:
                continue
            enable = next((m for m in found if m['mark'] == '?2004h' and m['off'] > disable['off']), None)
            if enable is None:
                continue
            frame = next((m for m in found if m['mark'] == '?2026h' and m['off'] > enable['off']), None)
            if arm == 'at-2004h' or frame is not None:
                disable_off, enable_off = disable['off'], enable['off']
                frame_off = frame['off'] if frame else None
                late_t, late_off = time.monotonic_ns(), len(s.raw)
                s.send('LATE', f'e1f-{arm}-late')
        s.wait(lambda: any(e['stage'] == 'exit' for e in self.entries()[n:]), 5)
        s.pump(.5)
        first = [e for e in self.entries()[n:] if e['stage'] == 'start']
        # The next export says exactly what the draft holds.
        check = self.invoke(s, '\x07', f'e1f-{arm}-check', {'action': 'keep'}, timeout=3)
        held = bytes.fromhex(check['start']['contents_hex']).decode(errors='replace') if check['opened'] else None
        window = marks(s.raw[sent_off:check['sent_off']], sent_off)
        return {'cell': 'e1f', 'arm': arm, 'trial': trial, 'sent_t': sent, 'sent_off': sent_off,
                'opened': bool(first), 'start_t': first[0]['t'] if first else None,
                'late_t': late_t, 'late_off': late_off, 'disable_off': disable_off, 'enable_off': enable_off,
                'frame_off': frame_off, 'marks': window, 'next_export': held, 'check_off': check['sent_off'],
                'kept': held is not None and 'LATE' in held}

    # ---- keymap (M58r) ----------------------------------------------------

    def keymap_cases(self):
        f6 = '[tui.keymap.global]\nopen_external_editor = "f6"\n'
        cg = '[tui.keymap.global]\nopen_external_editor = "ctrl-g"\n'
        model_only = 'model = "nollm-model"\n'
        cg_override = ['-c', 'tui.keymap.global.open_external_editor="ctrl-g"']
        f6_override = ['-c', 'tui.keymap.global.open_external_editor="f6"']
        legacy = '[profiles.measure]\nmodel = "nollm-model"\n'
        legacy_bind = '[profiles.measure.tui.keymap.global]\nopen_external_editor = "f6"\n'
        # (name, base config extra, files {relative to root: text}, args, symlinks)
        return [
            ('m58-1-model-only-profile', '', {'home/.codex/measure.config.toml': model_only}, ['-p', 'measure'], {}),
            ('m58-2-profile-binds-f6', '', {'home/.codex/measure.config.toml': f6}, ['-p', 'measure'], {}),
            ('m58-3-cli-restores-ctrl-g-over-profile', '', {'home/.codex/measure.config.toml': f6},
             ['-p', 'measure'] + cg_override, {}),
            ('m58-4-trusted-project-binds-f6', '', {'work/.codex/config.toml': f6}, [], {}),
            ('m58-5-unparsable-profile', '', {'home/.codex/measure.config.toml': 'model = = "x"\n'},
             ['-p', 'measure'], {}),
            ('m58-6-legacy-table-model-only', legacy, {}, ['-p', 'measure'], {}),
            ('m58-7-legacy-table-binding', legacy_bind, {}, ['-p', 'measure'], {}),
            ('m58-8-profile-file-and-legacy-table', legacy, {'home/.codex/measure.config.toml': model_only},
             ['-p', 'measure'], {}),
            ('prec-base-f6', f6, {}, [], {}),
            ('prec-base-f6-profile-ctrl-g', f6, {'home/.codex/measure.config.toml': cg}, ['-p', 'measure'], {}),
            ('prec-profile-f6-project-ctrl-g', '', {'home/.codex/measure.config.toml': f6,
                                                    'work/.codex/config.toml': cg}, ['-p', 'measure'], {}),
            ('prec-project-f6-profile-ctrl-g', '', {'home/.codex/measure.config.toml': cg,
                                                    'work/.codex/config.toml': f6}, ['-p', 'measure'], {}),
            ('prec-base-f6-project-ctrl-g', f6, {'work/.codex/config.toml': cg}, [], {}),
            ('prec-cli-f6-over-base-ctrl-g', cg, {}, f6_override, {}),
            ('prec-cli-f6-over-profile-ctrl-g', '', {'home/.codex/measure.config.toml': cg},
             ['-p', 'measure'] + f6_override, {}),
            ('prec-cli-f6-over-project-ctrl-g', '', {'work/.codex/config.toml': cg}, f6_override, {}),
            ('prec-cli-ctrl-g-over-project-f6', '', {'work/.codex/config.toml': f6}, cg_override, {}),
            ('symlinked-profile-binds-f6', '', {'home/target/measure.config.toml': f6}, ['-p', 'measure'],
             {'home/.codex/measure.config.toml': 'home/target/measure.config.toml'}),
            ('untrusted-project-binds-f6', f'[projects."{self.work / "sub"}"]\ntrust_level = "untrusted"\n',
             {'work/sub/.codex/config.toml': f6}, [], {'cwd': 'work/sub'}),
        ]

    def run_keymap(self):
        for name, extra, files, args, links in self.keymap_cases():
            self.seed(extra)
            target = self.root / 'home/target'
            if target.exists():
                shutil.rmtree(target)
            sub = self.work / 'sub'
            if sub.exists():
                shutil.rmtree(sub)
            cwd = self.work
            for rel, text in files.items():
                p = self.root / rel
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(text)
            for rel, to in links.items():
                if rel == 'cwd':
                    cwd = self.root / to
                    continue
                (self.root / rel).symlink_to(self.root / to)
            # What the deck would read: every file under the Codex home and
            # the work tree, with its contents, relative to the root.
            snapshot = {}
            for top in (self.codex_home, self.work, target):
                if top.exists():
                    for p in sorted(top.rglob('*')):
                        if p.is_file() and p.suffix == '.toml':
                            snapshot[str(p.relative_to(self.root))] = {
                                'text': p.read_text(), 'symlink': str(Path(os.readlink(p)).relative_to(self.root))
                                if p.is_symlink() else None}
            row = {'cell': 'keymap', 'case': name, 'args': BASE_ARGS + args, 'cwd': str(cwd.relative_to(self.root)),
                   'files': snapshot, 'root': str(self.root)}
            s = Session(self, f'keymap-{name}', {'VISUAL': self.editor_command('keymap')}, args, cwd=cwd)
            try:
                try:
                    ready = s.wait(s.ready, 20)
                    if ready:
                        s.pump(SETTLE)
                    if ready and 'Open restricted' in s.text():
                        # An untrusted folder's dialog, drawn after the
                        # first composer frame: open it restricted.
                        row['dialog'] = 'open-restricted'
                        s.send('\r', 'open-restricted')
                        ready = s.wait(lambda: 'Open restricted' not in s.text() and s.ready(), 10)
                        if ready:
                            s.pump(SETTLE)
                except Exited:
                    ready = False
                row['started'] = ready
                if not ready:
                    s.pump(.5) if s.exit_status is None else None
                    row['startup_screen'] = s.text()
                else:
                    s.send('CXM_KEYMAP', 'draft')
                    s.wait(lambda: s.holds('CXM_KEYMAP'), 2)
                    g = self.invoke(s, '\x07', f'{name}-ctrl-g', {'action': 'keep'}, timeout=2)
                    f = self.invoke(s, F6, f'{name}-f6', {'action': 'keep'}, timeout=2)
                    row.update(ctrl_g_opened=g['opened'], f6_opened=f['opened'],
                               ctrl_g_screen=g['screen_after'] if not g['opened'] else None)
            except Exited:
                row['started'] = False
                row['startup_screen'] = s.text()
            finally:
                try:
                    s.close()
                except OSError:
                    pass
            row['exit_status'] = s.exit_status
            self.rows.append(row)

    # ---- forms (M44e) -----------------------------------------------------

    def run_forms(self):
        self.seed()
        s = self.launch('forms', {'VISUAL': self.editor_command('forms')})
        try:
            for state in ('composer', 'slash', 'ctrlr', 'shortcuts'):
                for name, seq in FORMS:
                    for split in (False, True):
                        s.reset()
                        if state == 'slash':
                            s.send('/', 'slash-menu')
                        elif state == 'shortcuts':
                            s.send('?', 'shortcuts')
                        else:
                            s.send('MEASURE_DRAFT', 'draft')
                            s.wait(lambda: s.holds('MEASURE_DRAFT'), 2)
                            if state == 'ctrlr':
                                s.send('\x12', 'reverse-search')
                        s.pump(.3)
                        before = s.text()
                        row = self.invoke(s, seq, f'{state}-{name}', {'action': 'keep'}, timeout=1.2, split=split)
                        row.update(cell='forms', state=state, form=name, screen_before=before)
                        self.rows.append(row)
            for gap in (0, .03, .06):
                s.reset()
                s.send('MEASURE_DRAFT', 'draft')
                s.wait(lambda: s.holds('MEASURE_DRAFT'), 2)
                s.send('\x1b', 'esc-bel-gap')
                s.pump(gap)
                row = self.invoke(s, '\x07', 'esc-bel-gap', {'action': 'keep'}, timeout=1.2)
                row.update(cell='forms', state='composer', form='esc-bel-gap', gap_s=gap)
                self.rows.append(row)
            # P-PASTE: a bracketed BEL, whole and split; the next export's
            # bytes say whether the BEL stayed in the draft.
            for split in (False, True):
                for run in (1, 2):
                    s.reset()
                    s.send('BASE', 'draft')
                    s.wait(lambda: s.holds('BASE'), 2)
                    row = self.invoke(s, '\x1b[200~PASTE\x07TAIL\x1b[201~', 'paste-bel', {'action': 'keep'},
                                      timeout=1.2, split=split)
                    after = self.invoke(s, '\x07', 'paste-bel-export', {'action': 'keep'}, timeout=3)
                    row.update(cell='paste-bel', split=split, run=run,
                               next_export_hex=after['start']['contents_hex'] if after['opened'] else None)
                    self.rows.append(row)
        finally:
            s.close()

    def save(self, mode, error):
        result = {'release': self.release, 'sha256': self.sha256, 'executable': self.binary, 'uid': os.getuid(),
                  'mode': mode, 'error': error, 'rows': self.rows, 'notes': self.notes, 'size': [ROWS, COLS],
                  'term': os.environ.get('TERM'), 'colorterm': os.environ.get('COLORTERM'),
                  'env_names': sorted(os.environ), 'setup': 'nollm: dummy key, loopback Responses server, '
                  'bwrap --unshare-net, tmpfs /home /root /tmp /run/user',
                  'recorder_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
        (self.out / 'result.json').write_text(json.dumps(result, indent=1, ensure_ascii=False))


def inner(mode, root, binary, release, digest, pyte_path):
    global pyte
    sys.path[:0] = [p for p in pyte_path.split(':') if p]
    import pyte
    root = Path(root)
    assert Path(os.environ['HOME']) == root / 'home', 'requires the sandbox HOME'
    assert not any(k.startswith(('OPENAI_', 'ANTHROPIC_', 'CODEX_', 'CLAUDE_')) for k in os.environ), \
        'a credential-shaped variable reached the sandbox'
    assert not any(Path('/home').iterdir()), 'the real /home is visible'
    rec = Recorder(root, binary, release, digest)
    error = None
    try:
        {'editor': rec.run_editor, 'startup': rec.run_startup, 'config-writes': rec.run_config_writes,
         'e1f': rec.run_e1f, 'keymap': rec.run_keymap,
         'forms': rec.run_forms}[mode]()
    except Exception as exc:
        import traceback
        error = repr(exc) + '\n' + traceback.format_exc()
    finally:
        assert hashlib.sha256(Path(binary).read_bytes()).hexdigest() == digest, 'executable pin changed'
        rec.save(mode, error)


def main():
    if sys.argv[1] == '--editor':
        editor(Path(sys.argv[2]))
    elif sys.argv[1] == '--inner':
        inner(*sys.argv[2:8])
    else:
        outer(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else DEFAULT_CODEX)


if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        if sys.argv[1] == '--editor':
            print('measurement editor failed closed: ' + repr(exc), file=sys.stderr)
            sys.exit(2)
        raise
