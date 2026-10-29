#!/usr/bin/env python3
"""Retained PTY controls for the three synthetic composers; no deck or agent runs."""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
import uuid

SUPPORT = Path(__file__).resolve().parent
FIXTURES = ('claude_composer.py', 'codex_composer.py', 'empty_composer.py')
PREFIX = 'DOT_AGENT_DECK_TEST_COMPOSER_'

# Instrument the running fixture at I/O boundaries, without fixture-only race
# knobs. A parser barrier lets the parent replace the pending path after open
# (old code) or after claim (fixed code). Loop ticks prove an unpublished write
# has been polled past. The public-path guard also catches read-before-claim.
BOOTSTRAP = r"""
import builtins, contextlib, json, os, runpy, select, sys, time
script = sys.argv.pop(1)
sys.path.insert(0, os.path.dirname(script))
base = sys.argv[1]
original_open, original_load, original_select = builtins.open, json.load, select.select
original_write = os.write
def checked_write(fd, data):
    if fd == 1 and os.environ.get('CHECK_DRIVER_WRITES') == '1':
        with original_open(base + '.writes', 'a') as log:
            log.write(json.dumps(list(data)) + '\n')
    return original_write(fd, data)
os.write = checked_write
first = True
tick = 0
def checked_open(path, mode='r', *args, **kwargs):
    if os.environ.get('CHECK_PUBLIC_READ') == '1':
        assert str(path) != base + '.bash-tool.json' or 'r' not in mode or not os.path.exists(path), 'parser opened public request'
        assert str(path) != base + '.bash-tool-result.json' or not any(c in mode for c in 'wax'), 'result written at public name'
    return original_open(path, mode, *args, **kwargs)
def paused_load(stream, *args, **kwargs):
    global first
    if first and os.environ.get('PAUSE_PARSE') == '1' and '.bash-tool.json' in str(stream.name):
        first = False
        with original_open(base + '.parse-ready', 'w') as ready:
            ready.write('ready')
        deadline = time.monotonic() + 10
        while not os.path.exists(base + '.parse-go'):
            assert time.monotonic() < deadline, 'parent did not release parser'
            time.sleep(0.005)
    return original_load(stream, *args, **kwargs)
@contextlib.contextmanager
def tick_output(path):
    with original_open(path, 'w') as output:
        if tick == 1 and os.environ.get('PAUSE_TICK') == '1':
            with original_open(base + '.tick-ready', 'w') as ready:
                ready.write('ready')
            deadline = time.monotonic() + 10
            while not os.path.exists(base + '.tick-go'):
                assert time.monotonic() < deadline, 'parent did not release tick writer'
                time.sleep(0.005)
        yield output
def tick_select(*args, **kwargs):
    global tick
    tick += 1
    with tick_output(base + '.tick.tmp') as output:
        output.write(str(tick))
    os.replace(base + '.tick.tmp', base + '.tick')
    return original_select(*args, **kwargs)
builtins.open, json.load, select.select = checked_open, paused_load, tick_select
runpy.run_path(script, run_name='__main__')
"""


class Composer:
    def __init__(self, root, fixture, extra=None, instrument=False, rows=40, cols=140):
        self.base = root / fixture
        env = {'PATH': os.environ['PATH'], 'HOME': str(root),
               'TERM': 'xterm-256color', 'LANG': 'C.UTF-8'}
        env.update(extra or {})
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        script = str(SUPPORT / fixture)
        tail = [str(self.base)]
        if fixture != 'empty_composer.py':
            tail += ['/bin/true', '0.1.2']
        self.argv = [script, *tail]
        launch = [sys.executable, script, *tail]
        if instrument:
            launch = [sys.executable, '-c', BOOTSTRAP, script, *tail]
        self.proc = subprocess.Popen(launch, stdin=slave, stdout=slave, stderr=slave,
                                     cwd=root, env=env)
        os.close(slave)
        self.output = b''

    def path(self, suffix):
        return Path(str(self.base) + suffix)

    def drain(self):
        while select.select([self.master], [], [], 0)[0]:
            try:
                self.output += os.read(self.master, 65536)
            except OSError:
                break

    def wait(self, predicate, message):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            self.drain()
            assert self.proc.poll() is None, (
                f'{message}: fixture exited {self.proc.returncode}: {self.output!r}')
            value = predicate()
            if value:
                return value
            time.sleep(0.005)
        raise AssertionError(f'{message}: timed out: {self.output!r}')

    def read_json(self, suffix):
        try:
            return json.loads(self.path(suffix).read_text())
        except FileNotFoundError:
            return None

    def ready(self):
        frame_end = {
            'claude_composer.py': b'\x1b[26;3H',
            'codex_composer.py': b'\x1b[?2026l',
            'empty_composer.py': b'COMPOSER: \x1b[2m<empty>\x1b[22m',
        }[self.base.name]
        previous, stable_since = None, time.monotonic()
        def complete_stable_frame():
            nonlocal previous, stable_since
            if self.output != previous:
                previous, stable_since = self.output, time.monotonic()
            return (self.path('.pid').exists() and self.output.endswith(frame_end)
                    and time.monotonic() - stable_since >= 0.025)
        self.wait(complete_stable_frame, 'complete stable initial frame')
        return self.output

    def cycle(self):
        def tick():
            try:
                return int(self.path('.tick').read_text())
            except (FileNotFoundError, ValueError):
                return None
        before = self.wait(tick, 'first complete polling tick')
        self.wait(lambda: (current := tick()) is not None and current > before + 1,
                  'two complete polling cycles')

    def publish(self, request):
        pending = self.path('.bash-tool.json')
        assert not pending.exists(), 'publisher must not overwrite a pending request'
        temporary = self.path('.bash-tool.json.tmp-' + uuid.uuid4().hex)
        temporary.write_text(json.dumps(request))
        temporary.replace(pending)

    def result(self, request_id):
        result = self.wait(lambda: self.read_json('.bash-tool-result.json'), 'tool result')
        assert result['id'] == request_id, result
        return result

    def acknowledge(self):
        self.path('.bash-tool-result.json').unlink()

    def close(self):
        if self.proc.poll() is None:
            self.proc.terminate()
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=3)
        os.close(self.master)


class ComposerKnobs(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='composer-knobs-', dir='/var/tmp')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def start(self, fixture, extra=None, instrument=False):
        root = self.root / str(len(list(self.root.iterdir())))
        root.mkdir()
        composer = Composer(root, fixture, extra, instrument)
        self.addCleanup(composer.close)
        composer.ready()
        return composer

    def test_default_off_and_boolean_zero(self):
        """Scenario: launch each screen with unset or zero knobs; its frame and optional files stay unchanged."""
        for fixture in FIXTURES:
            baseline = self.start(fixture).output
            for setting in (None, '0'):
                with self.subTest(fixture=fixture, setting=setting):
                    extra = {} if setting is None else {
                        PREFIX + 'REPORT': setting, PREFIX + 'BASH_TOOL': setting}
                    composer = self.start(fixture, extra, instrument=True)
                    self.assertEqual(composer.output, baseline)
                    composer.publish({'id': 'disabled', 'argv': ['/bin/false']})
                    composer.cycle()
                    self.assertTrue(composer.path('.bash-tool.json').exists())
                    self.assertFalse(composer.path('.spawn.json').exists())
                    self.assertFalse(composer.path('.bash-tool-result.json').exists())

    def test_spawn_report_distinguishes_unset_empty_and_set(self):
        """Scenario: start with absent, empty and populated editor variables; reports preserve argv and those distinctions."""
        for fixture in FIXTURES:
            for value in (None, '', "user editor ' $ ;"):
                with self.subTest(fixture=fixture, value=value):
                    extra = {PREFIX + 'REPORT': '1'}
                    if value is not None:
                        extra.update(dict.fromkeys(('VISUAL', 'EDITOR', 'CLAUDE_ENV_FILE'), value))
                    composer = self.start(fixture, extra)
                    self.assertEqual(composer.read_json('.spawn.json'), {
                        'argv': composer.argv,
                        'env': dict.fromkeys(('VISUAL', 'EDITOR', 'CLAUDE_ENV_FILE'), value)})

    def test_release_restoration_literal_arguments_and_tty(self):
        """Scenario: source a trusted oddly named env file in each tool shell; restore editors, preserve literal argv and keep tty stdio."""
        for fixture in FIXTURES:
            for target in (None, '', "editor with spaces ' $ ;"):
                with self.subTest(fixture=fixture, target=target):
                    envfile = self.root / "env ' $(literal) ;.sh"
                    envfile.write_text('unset VISUAL EDITOR\n' if target is None else
                                       'export VISUAL=' + shlex.quote(target) +
                                       '\nexport EDITOR=' + shlex.quote(target) + '\n')
                    extra = {PREFIX + 'RELEASE': '9.8.7', PREFIX + 'REPORT': '1',
                             PREFIX + 'BASH_TOOL': '1', 'VISUAL': '/helper/visual',
                             'EDITOR': '/helper/editor', 'CLAUDE_ENV_FILE': str(envfile)}
                    composer = self.start(fixture, extra)
                    self.assertIn(b'v9.8.7', composer.output)
                    restored = composer.path('.restored.json')
                    command = [sys.executable, '-c',
                               'import json,os,sys; open(sys.argv[1],"w").write(json.dumps({'
                               '"VISUAL":os.getenv("VISUAL"),"EDITOR":os.getenv("EDITOR"),'
                               '"tty":[os.isatty(n) for n in range(3)],"arg":sys.argv[2]}))',
                               str(restored), "literal ' $ ; $(not-a-command)"]
                    composer.publish({'id': 'restore', 'argv': command})
                    self.assertEqual(composer.result('restore'), {'id': 'restore', 'exit': 0})
                    self.assertEqual(json.loads(restored.read_text()), {
                        'VISUAL': target, 'EDITOR': target, 'tty': [True, True, True],
                        'arg': command[-1]})
                    self.assertEqual(composer.read_json('.spawn.json')['env'], {
                        'VISUAL': '/helper/visual', 'EDITOR': '/helper/editor',
                        'CLAUDE_ENV_FILE': str(envfile)})

    def test_split_publication_and_private_parser_and_result(self):
        """Scenario: poll past two unfinished temp writes, then publish; only the claimed complete command runs and its complete result appears."""
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture):
                composer = self.start(fixture, {PREFIX + 'BASH_TOOL': '1',
                                                'CHECK_PUBLIC_READ': '1'}, instrument=True)
                temporary = composer.path('.bash-tool.json.tmp-split')
                with temporary.open('w') as output:
                    output.write('{"id":"split",')
                    output.flush()
                    composer.cycle()
                    self.assertFalse(composer.path('.bash-tool-result.json').exists())
                    output.write('"argv":["/bin/true"]}')
                    output.flush()
                    composer.cycle()
                    self.assertFalse(composer.path('.bash-tool-result.json').exists())
                temporary.replace(composer.path('.bash-tool.json'))
                self.assertEqual(composer.result('split'), {'id': 'split', 'exit': 0})

    def test_replacement_at_parse_boundary_and_result_acknowledgement(self):
        """Scenario: publish B while A is paused after claim; A and B each run once, with B waiting for A's result acknowledgement."""
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture):
                composer = self.start(fixture, {PREFIX + 'BASH_TOOL': '1',
                                                'PAUSE_PARSE': '1'}, instrument=True)
                counts = composer.path('.commands')
                def request(identifier, status):
                    return {'id': identifier, 'argv': [sys.executable, '-c',
                            'import sys; open(sys.argv[1],"a").write(sys.argv[2]+"\\n"); '
                            'sys.exit(int(sys.argv[3]))', str(counts), identifier, str(status)]}
                composer.publish(request('A', 3))
                composer.wait(lambda: composer.path('.parse-ready').exists(), 'parser barrier')
                self.assertFalse(composer.path('.bash-tool.json').exists(), 'request must be claimed before parsing')
                composer.publish(request('B', 7))
                composer.path('.parse-go').touch()
                self.assertEqual(composer.result('A'), {'id': 'A', 'exit': 3})
                composer.cycle()
                self.assertEqual(counts.read_text().splitlines(), ['A'])
                self.assertTrue(composer.path('.bash-tool.json').exists(), 'B must survive A')
                composer.acknowledge()
                self.assertEqual(composer.result('B'), {'id': 'B', 'exit': 7})
                composer.cycle()
                self.assertEqual(counts.read_text().splitlines(), ['A', 'B'])
                composer.acknowledge()
                composer.cycle()
                self.assertEqual(counts.read_text().splitlines(), ['A', 'B'])

    def test_malformed_claim_returns_error_and_next_command_recovers(self):
        """Scenario: publish invalid JSON, empty argv, NUL-bearing argv or unencodable argv; receive a defined error with the request id preserved, acknowledge it, then successfully run another command."""
        for fixture in FIXTURES:
            for content, identifier in (('{"id":', None),
                                         (json.dumps({'id': 'bad', 'argv': []}), 'bad'),
                                         (json.dumps({'id': 'bad-nul', 'argv': ['/bin/true', '\x00']}), 'bad-nul'),
                                         (json.dumps({'id': 'bad-encoding', 'argv': ['/bin/true', '\ud800']}), 'bad-encoding')):
                with self.subTest(fixture=fixture, identifier=identifier):
                    composer = self.start(fixture, {PREFIX + 'BASH_TOOL': '1'}, instrument=True)
                    temporary = composer.path('.bash-tool.json.tmp-malformed')
                    temporary.write_text(content)
                    temporary.replace(composer.path('.bash-tool.json'))
                    result = composer.result(identifier)
                    self.assertIsNone(result['exit'])
                    self.assertEqual(result['error'], 'invalid_request')
                    composer.acknowledge()
                    composer.publish({'id': 'recovered', 'argv': ['/bin/true']})
                    self.assertEqual(composer.result('recovered'), {'id': 'recovered', 'exit': 0})

    def test_poll_ticks_publish_complete_counters(self):
        """Scenario: pause each composer's tick writer after opening its output; readers see no published counter until it is complete, then witness newer polls."""
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture):
                composer = self.start(fixture, {'PAUSE_TICK': '1'}, instrument=True)
                composer.wait(lambda: composer.path('.tick-ready').exists(), 'tick writer barrier')
                published = composer.path('.tick')
                try:
                    if published.exists():
                        self.assertRegex(published.read_text(), r'^[1-9][0-9]*$', 'published tick must be complete')
                finally:
                    composer.path('.tick-go').touch()
                composer.cycle()


class DriverKnobs(unittest.TestCase):
    """SC-06 controls, independent of the existing mailbox controls."""

    setUp = ComposerKnobs.setUp

    def start_driver(self, agent, extra=None, instrument=False):
        root = self.root / str(len(list(self.root.iterdir())))
        root.mkdir()
        composer = Composer(root, agent + '_composer.py', extra, instrument,
                            rows=50, cols=200)
        self.addCleanup(composer.close)
        last = [b'', time.monotonic()]
        def settled():
            if composer.output != last[0]:
                last[:] = [composer.output, time.monotonic()]
            return (composer.path('.pid').exists() and composer.output and
                    time.monotonic() - last[1] >= .04)
        composer.wait(settled, 'initial driver frame')
        return composer

    def changed_frame(self, composer, action):
        before = composer.output
        last = [before, time.monotonic()]
        action()
        def settled():
            if composer.output != last[0]:
                last[:] = [composer.output, time.monotonic()]
            return composer.output != before and time.monotonic() - last[1] >= .04
        composer.wait(settled, 'settled changed frame')
        return composer.output[len(before):].decode()

    def test_driver_defaults_match_frozen_parent_frames(self):
        """Scenario: start, type, delete and resize with the new knobs unset or disabled; all emitted bytes equal the frozen pre-change stand-ins."""
        frames = json.loads((SUPPORT.parent / 'fixtures/release-check/driver-frames.json').read_text())
        for agent in ('claude', 'codex'):
            for extra in ({}, {PREFIX + 'DRIVER_FAULT': '', PREFIX + 'TIMING_RESTORE': '0'}):
                with self.subTest(agent=agent, extra=extra):
                    composer = self.start_driver(agent, extra)
                    self.assertEqual(composer.output.decode(), frames[agent]['ready'])
                    self.assertEqual(self.changed_frame(composer, lambda: os.write(composer.master, b'(abcdef) synthetic')),
                                     frames[agent]['typed'])
                    self.assertEqual(self.changed_frame(composer, lambda: os.write(composer.master, b'\x7f' * 18)),
                                     frames[agent]['deleted'])
                    self.assertEqual(self.changed_frame(composer, lambda: fcntl.ioctl(
                        composer.master, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))),
                                     frames[agent]['resized'])

    def test_driver_faults_change_bytes_inside_one_write(self):
        """Scenario: raise each screen fault on each composer; its raw operations are present in the sole initial write, including a drawn-and-erased overlay."""
        faults = {'overlay': b'\x1b[35;5H!\x1b[2K',
                  'moved_glyph': b'\x1b[35;5HC\x1b[2K',
                  'mode': b'\x1b[?1000h\x1b[?1000l',
                  'control': b'\x1b]52;c;YQ==\x07',
                  'modal': b'\x1b[35;5HConfirm?'}
        for agent in ('claude', 'codex'):
            for fault, inserted in faults.items():
                with self.subTest(agent=agent, fault=fault):
                    composer = self.start_driver(agent, {PREFIX + 'DRIVER_FAULT': fault,
                                                         'CHECK_DRIVER_WRITES': '1'}, instrument=True)
                    writes = [bytes(json.loads(line)) for line in composer.path('.writes').read_text().splitlines()]
                    self.assertEqual(writes, [composer.output])
                    self.assertIn(inserted, writes[0])
                    if agent == 'codex':
                        self.assertLess(writes[0].index(inserted), writes[0].index(b'\x1b[?2026l'))

    def test_driver_echo_and_termios_faults_are_observable(self):
        """Scenario: type a line with echo drawing disabled, then start with terminal echo enabled; the missing text and wrong slave mode can be measured independently."""
        for agent in ('claude', 'codex'):
            composer = self.start_driver(agent, {PREFIX + 'DRIVER_FAULT': 'missing_echo'})
            frame = self.changed_frame(composer, lambda: os.write(composer.master, b'(abcdef) synthetic'))
            self.assertNotIn('(abcdef) synthetic', frame)
            self.assertFalse(termios.tcgetattr(composer.master)[3] & (termios.ICANON | termios.ECHO))
            wrong_mode = self.start_driver(agent, {PREFIX + 'DRIVER_FAULT': 'echo_mode'})
            self.assertTrue(termios.tcgetattr(wrong_mode.master)[3] & termios.ECHO)
            self.assertFalse(termios.tcgetattr(wrong_mode.master)[3] & termios.ICANON)

    def test_driver_timing_restore_preserves_old_cells_until_typing(self):
        """Scenario: type into Claude's empty composer; before the typed redraw one write erases and restores only the old placeholder's own cells."""
        composer = self.start_driver('claude', {PREFIX + 'TIMING_RESTORE': '1'})
        frames = json.loads((SUPPORT.parent / 'fixtures/release-check/driver-frames.json').read_text())
        self.assertEqual(composer.output.decode(), frames['claude']['ready'])
        frame = self.changed_frame(composer, lambda: os.write(composer.master, b'(abcdef) synthetic'))
        self.assertEqual(frame, '\x1b[26;3H\x1b[2X\x1b[H\x1b[26;3H\x1b[2mTr\x1b[22m\x1b[H' + frames['claude']['typed'])


if __name__ == '__main__':
    unittest.main(verbosity=2)

