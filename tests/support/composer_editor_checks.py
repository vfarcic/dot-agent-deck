#!/usr/bin/env python3
"""Private PTY fixture controls; imports no product code and starts no agent."""
import json
import os
from pathlib import Path
import sys
import tempfile
import signal
import time
import unittest

from composer_knob_checks import Composer, FIXTURES, PREFIX, SUPPORT


class ExternalEditor(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='composer-edit-', dir='/var/tmp')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.editor = self.root / 'editor'
        self.editor.write_text((SUPPORT / 'composer_test_editor.py').read_text())
        self.editor.chmod(0o700)

    def start(self, fixture, mode='fast', extra=None):
        root = self.root / str(len(list(self.root.iterdir())))
        root.mkdir()
        base = root / fixture
        env = {PREFIX + 'EXTERNAL_EDIT': '1', 'TMPDIR': str(root),
               'CODEX_HOME': str(root / 'codex'),
               'VISUAL': f'{self.editor} {mode} {base}'}
        env.update(extra or {})
        env = {key: value.replace('{base}', str(base))
               for key, value in env.items() if value is not None}
        composer = Composer(root, fixture, env, instrument=True)
        self.addCleanup(composer.close)
        def stop_editor():
            if not composer.path('.editor-finished').exists():
                try:
                    os.kill(composer.read_json('.editor-started')['pid'], signal.SIGKILL)
                except (FileNotFoundError, ProcessLookupError, TypeError, json.JSONDecodeError):
                    pass
        self.addCleanup(stop_editor)
        composer.ready()
        return composer

    def type(self, composer, data):
        os.write(composer.master, data)

    def started(self, composer):
        # The editor writes this small record before waiting; retry partial reads.
        def read():
            try:
                return composer.read_json('.editor-started')
            except json.JSONDecodeError:
                return None
        return composer.wait(read, 'external editor started')

    def resumed(self, composer):
        composer.wait(lambda: b'\x1b[?2004h' in composer.output[
            composer.output.rfind(b'\x1b[?2004l'):], 'resume enable after disable')
        composer.cycle()

    def draft(self, composer):
        composer.path('.editor-started').unlink(missing_ok=True)
        self.type(composer, b'\x07')
        return bytes.fromhex(self.started(composer)['hex'])

    def test_default_off(self):
        """Scenario: leave external editing unset or zero, send Ctrl+G, and see no editor or suspension."""
        for fixture in FIXTURES:
            for value in (None, '0'):
                with self.subTest(fixture=fixture, value=value):
                    extra = {PREFIX + 'EXTERNAL_EDIT': value}
                    composer = self.start(fixture, extra=extra)
                    self.type(composer, b'BASE\x07')
                    composer.cycle()
                    self.assertFalse(composer.path('.editor-started').exists())
                    self.assertNotIn(b'\x1b[?2004l', composer.output)

    def test_export_markers_reimport_and_failed_codex_preservation(self):
        """Scenario: export a draft through a successful or failed editor; inspect its file and the resumed visible composer."""
        for fixture in FIXTURES:
            for mode in ('fast', 'fail'):
                with self.subTest(fixture=fixture, mode=mode):
                    composer = self.start(fixture, mode)
                    self.type(composer, b'BASE   \x07')
                    record = self.started(composer)
                    self.assertEqual(bytes.fromhex(record['hex']), b'BASE   ')
                    self.assertEqual(record['tty'], [True] * 3)
                    path = Path(record['path'])
                    if fixture == 'codex_composer.py':
                        self.assertEqual(path.parent, composer.base.parent / 'codex/editor')
                        self.assertRegex(path.name, r'^\.tmp[A-Za-z0-9]{6}\.md$')
                        self.assertEqual(record['mode'], 0o600)
                    else:
                        self.assertEqual(path.parent, composer.base.parent / f'claude-{os.getuid()}')
                        self.assertRegex(path.name, r'^claude-prompt-[0-9a-f-]{36}\.md$')
                        self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
                    self.resumed(composer)
                    expected = b'BASE   ' if mode == 'fail' else b'EDITED'
                    composer.wait(lambda: expected in composer.output.split(b'\x1b[?2004h')[-1],
                                  'returned draft is visibly drawn')
                    expected_disable = 2 if fixture == 'codex_composer.py' else 1
                    self.assertEqual(composer.output.count(b'\x1b[?2004l'), expected_disable)
                    self.assertEqual(composer.output.count(b'\x1b[?2004h'), 2)
                    if fixture == 'codex_composer.py' and mode == 'fail':
                        self.assertIn(b'Failed to open editor: editor exited with status exit status: 1',
                                      composer.output.split(b'\x1b[?2026h')[-1])

    def test_agent_editor_selection_and_literal_arguments(self):
        """Scenario: vary absent, empty and quoted editor commands; Claude falls back on emptiness while Codex reports an error."""
        for fixture in FIXTURES:
            for visual in (None, '', 'chosen'):
                with self.subTest(fixture=fixture, visual=visual):
                    # A single-quoted argument is literal text in Claude's split,
                    # and grouped by Codex. No shell expansion should occur.
                    command = f'{self.editor} fast {{base}} ;'
                    composer = self.start(fixture, extra={
                        'EDITOR': command,
                        'VISUAL': None if visual is None else
                                  ('' if visual == '' else command + " 'two words'"),
                    })
                    self.type(composer, b'BASE\x07')
                    if fixture == 'codex_composer.py' and visual == '':
                        composer.wait(lambda: b'empty editor command' in composer.output, 'Codex empty VISUAL error')
                        self.assertFalse(composer.path('.editor-started').exists())
                        continue
                    record = self.started(composer)
                    self.assertIn(';', record['argv'])
                    if visual == 'chosen':
                        expected = ['two words'] if fixture == 'codex_composer.py' else ["'two", "words'"]
                        self.assertEqual(record['argv'][1:-1], expected)

    def test_claude_editor_families_and_default_search(self):
        """Scenario: select measured editor basenames and each default-search fallback from a private PATH; the editor receives the expected line-number arguments without opening a real installed editor."""
        binaries = self.root / 'bin'
        binaries.mkdir()
        (binaries / 'python3').symlink_to(sys.executable)
        launcher = (f'#!{sys.executable}\nimport os,sys\n'
                    f'editor = {str(self.editor)!r}\n'
                    'os.execv(editor, [editor, "fast", os.environ["FIXTURE_BASE"], *sys.argv[1:]])\n')
        for name in ('code', 'cursor', 'windsurf', 'codium', 'subl', 'nvim', 'gedit', 'vi', 'nano'):
            target = binaries / name
            target.write_text(launcher)
            target.chmod(0o700)
        for fixture in ('claude_composer.py', 'empty_composer.py'):
            for name in ('code', 'cursor', 'windsurf', 'codium', 'subl', 'nvim', 'gedit'):
                with self.subTest(fixture=fixture, editor=name):
                    composer = self.start(fixture, extra={
                        'PATH': str(binaries), 'FIXTURE_BASE': '{base}', 'VISUAL': name})
                    self.type(composer, b'BASE\x07')
                    record = self.started(composer)
                    self.resumed(composer)
                    expected = {'code': ['-w', '-g'], 'cursor': ['-g'], 'windsurf': ['-g'],
                                'codium': ['-g'], 'subl': ['--wait'], 'nvim': ['+1'],
                                'gedit': []}[name]
                    self.assertEqual(record['argv'][:-1], expected)
                    self.assertEqual(record['argv'][-1].endswith(':1'), name in
                                     ('code', 'cursor', 'windsurf', 'codium', 'subl'))
        for chosen in ('code', 'vi', 'nano'):
            for fixture in FIXTURES:
                with self.subTest(fixture=fixture, default=chosen):
                    composer = self.start(fixture, extra={
                        'PATH': str(binaries), 'FIXTURE_BASE': '{base}',
                        'VISUAL': None, 'EDITOR': None})
                    self.type(composer, b'BASE\x07')
                    if fixture == 'codex_composer.py':
                        composer.wait(lambda: b'set $VISUAL or $EDITOR' in composer.output,
                                      'Codex has no default editor')
                        self.assertFalse(composer.path('.editor-started').exists())
                    else:
                        record = self.started(composer)
                        self.assertEqual(record['argv'][:-1], ['-w', '-g'] if chosen == 'code' else ['+1'])
                        self.resumed(composer)
            (binaries / chosen).unlink()

    def test_delay_and_each_loss_mechanism(self):
        """Scenario: send a tail in the Ctrl+G read or during dispatch; each configured mechanism discards its own tail and a control keeps it."""
        for fixture in FIXTURES:
            for mechanism in ('none', 'H-chunk', 'H-flush', 'H-ignore'):
                with self.subTest(fixture=fixture, mechanism=mechanism):
                    composer = self.start(fixture)
                    composer.path('.edit-mechanism').write_text(mechanism)
                    composer.path('.edit-delay').touch()
                    before = time.monotonic()
                    self.type(composer, b'BASE\x07' + (b'TAIL' if mechanism == 'H-chunk' else b''))
                    composer.wait(lambda: composer.path('.edit-dispatched').exists(), 'candidate read before delay')
                    self.assertFalse(composer.path('.editor-started').exists())
                    if mechanism != 'H-chunk':
                        self.type(composer, b'TAIL')
                    record = self.started(composer)
                    self.assertGreaterEqual(time.monotonic() - before, 0.5)
                    self.assertEqual(bytes.fromhex(record['hex']), b'BASE')
                    self.resumed(composer)
                    result = self.draft(composer)
                    kept = mechanism == 'none' and fixture != 'codex_composer.py'
                    self.assertEqual(result, b'EDITEDTAIL' if kept else b'EDITED')

    def test_dropped_candidate_and_ignored_human_then_retry(self):
        """Scenario: drop a candidate persistently or ignore it once, then retry; no editor opens for the ignored key and the retry exports the untouched draft."""
        for fixture in FIXTURES:
            for knob in ('.edit-drop', '.edit-ignore-next'):
                with self.subTest(fixture=fixture, knob=knob):
                    composer = self.start(fixture)
                    composer.path(knob).touch()
                    self.type(composer, b'BASE\x07TAIL')
                    composer.cycle()
                    self.assertFalse(composer.path('.editor-started').exists())
                    self.assertNotIn(b'\x1b[?2004l', composer.output)
                    composer.path(knob).unlink(missing_ok=True)
                    self.assertEqual(self.draft(composer), b'BASETAIL')

    def test_wait_hung_editor_and_user_editor_stdin(self):
        """Scenario: keep a genuine fixture editor running across polling scans, send it a line, and release it explicitly; no resume appears while it runs."""
        for fixture in FIXTURES:
            for mode in ('hang', 'read'):
                with self.subTest(fixture=fixture, mode=mode):
                    composer = self.start(fixture, mode)
                    self.type(composer, b'BASE\x07')
                    self.started(composer)
                    self.type(composer, b'TO_EDITOR\n')
                    if mode == 'read':
                        composer.wait(lambda: composer.path('.editor-input').exists() and
                                      composer.path('.editor-input').read_bytes() == b'TO_EDITOR\n',
                                      'editor receives subsequent keys')
                    time.sleep(0.1)  # crosses two 50 ms scans; this editor has no timeout
                    composer.drain()
                    self.assertNotIn(b'\x1b[?2004h', composer.output[
                        composer.output.rfind(b'\x1b[?2004l'):])
                    self.assertFalse(composer.path('.editor-finished').exists())
                    # Do not leave input from the hung non-reader in the composer.
                    if mode == 'hang':
                        import termios
                        termios.tcflush(composer.master, termios.TCIFLUSH)
                    composer.path('.editor-go').touch()
                    self.resumed(composer)

    def test_encoded_candidates_split_paste_and_successful_trim(self):
        """Scenario: send encoded Ctrl+G keys, including split reads, then a paste containing BEL; exports distinguish the shortcut from pasted text and pin successful Codex trimming."""
        for fixture in FIXTURES:
            for form in (b'\x07', b'\x1b[103;5u', b'\x1b[103;5:1u',
                         b'\x1b[103;5:2u', b'\x1b[27;5;103~'):
                with self.subTest(fixture=fixture, form=form):
                    composer = self.start(fixture, 'fail')
                    self.type(composer, b'BASE' + form[:2])
                    composer.cycle()
                    if len(form) > 2:
                        self.assertFalse(composer.path('.editor-started').exists())
                    self.type(composer, form[2:])
                    self.assertEqual(bytes.fromhex(self.started(composer)['hex']), b'BASE')
                    self.resumed(composer)
            with self.subTest(fixture=fixture, paste=True):
                composer = self.start(fixture, 'fail')
                self.type(composer, b'\x1b[200~PASTE\x07TAIL\x1b[20')
                composer.cycle()
                self.assertFalse(composer.path('.editor-started').exists())
                self.type(composer, b'1~\x07')
                expected = b'PASTETAIL' if fixture == 'codex_composer.py' else b'PASTE\x07TAIL'
                self.assertEqual(bytes.fromhex(self.started(composer)['hex']), expected)
                self.resumed(composer)
            with self.subTest(fixture=fixture, trim=True):
                composer = self.start(fixture, 'trim')
                self.type(composer, b'BASE\x07')
                self.started(composer)
                self.resumed(composer)
                expected = b'EDITED' if fixture == 'codex_composer.py' else b'EDITED   \n'
                self.assertEqual(self.draft(composer), expected)

    def test_immediate_editor_is_gone_before_the_first_scan(self):
        """Scenario: let the immediate editor finish before a deliberately delayed first 50 ms scan; the draft and resume marker still appear even though that scan cannot observe the editor running."""
        for fixture in FIXTURES:
            with self.subTest(fixture=fixture):
                composer = self.start(fixture)
                self.type(composer, b'BASE\x07')
                record = self.started(composer)
                self.resumed(composer)  # the stand-in has waited/reaped the child
                time.sleep(0.05)  # first observer scan is deliberately after exit
                with self.assertRaises(ProcessLookupError):
                    os.kill(record['pid'], 0)
                self.assertTrue(composer.path('.editor-finished').exists())
                self.assertEqual(self.draft(composer), b'EDITED')


if __name__ == '__main__':
    unittest.main(verbosity=2)
