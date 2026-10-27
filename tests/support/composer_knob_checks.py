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
import builtins, json, os, runpy, select, sys, time
script = sys.argv.pop(1)
base = sys.argv[1]
original_open, original_load, original_select = builtins.open, json.load, select.select
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
def tick_select(*args, **kwargs):
    global tick
    tick += 1
    with original_open(base + '.tick', 'w') as output:
        output.write(str(tick))
    return original_select(*args, **kwargs)
builtins.open, json.load, select.select = checked_open, paused_load, tick_select
runpy.run_path(script, run_name='__main__')
"""


class Composer:
    def __init__(self, root, fixture, extra=None, instrument=False):
        self.base = root / fixture
        env = {'PATH': os.environ['PATH'], 'HOME': str(root),
               'TERM': 'xterm-256color', 'LANG': 'C.UTF-8'}
        env.update(extra or {})
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
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
        self.wait(lambda: self.path('.pid').exists() and self.output, 'initial frame')
        return self.output

    def cycle(self):
        def tick():
            try:
                return int(self.path('.tick').read_text())
            except (FileNotFoundError, ValueError):
                return 0
        before = tick()
        self.wait(lambda: tick() > before + 1, 'two complete polling cycles')

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
        """Scenario: publish invalid JSON or argv; receive a defined error, acknowledge it, then successfully run another command."""
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture):
                composer = self.start(fixture, {PREFIX + 'BASH_TOOL': '1'}, instrument=True)
                for content, identifier in (('{"id":', None),
                                             (json.dumps({'id': 'bad', 'argv': []}), 'bad')):
                    temporary = composer.path('.bash-tool.json.tmp-malformed')
                    temporary.write_text(content)
                    temporary.replace(composer.path('.bash-tool.json'))
                    result = composer.result(identifier)
                    self.assertIsNone(result['exit'])
                    self.assertEqual(result['error'], 'invalid_request')
                    composer.acknowledge()
                composer.publish({'id': 'recovered', 'argv': ['/bin/true']})
                self.assertEqual(composer.result('recovered'), {'id': 'recovered', 'exit': 0})


if __name__ == '__main__':
    unittest.main(verbosity=2)

