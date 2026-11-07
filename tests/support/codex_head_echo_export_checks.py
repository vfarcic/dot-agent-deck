#!/usr/bin/env python3
"""Unit checks for `codex_head_echo_export.py`'s deny scan, run by the fast
tier (`tests/codex_head_echo_evidence.rs`). No agent, no network: synthetic
bytes, one committed capture read as data, and a temporary capture directory.
"""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location('codex_head_echo_export', HERE / 'codex_head_echo_export.py')
export = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(export)
SMALL_1 = HERE.parent / 'fixtures/codex-inbox/release-0.164.0/codex-0.164.0-head-echo-staged-small-1'


def kinds(text, **kw):
    return sorted({what for _, what, _ in export.scan({'t': text.encode()}, **kw)})


class DenyScan(unittest.TestCase):
    def test_what_the_committed_captures_draw_passes(self):
        for text in [
            '› (abcdfg) dot-agent-deck: message m-abcdefgh-0123456789abcdef waiting, run: dot-agent-deck inbox fetch',
            'codex --sandbox workspace-write --ask-for-approval never',
            '/var/tmp/dad-tests-1234-abc/home/.codex/config.toml',
            'b93f6811d3f653f0fb83be4050d857fb9abbb111dfa2ce160563327daff7f306',
            '/usr/local/lib/node_modules/@openai/codex/bin/codex.js',
            'vfarcic vfarcic 600 hook.sock',
            '-rw------- 1 root root 0 Nov  4 hook.sock',
            'the slowest head echo used 4.9% of its wait, within the 600 ms',
            'User-Agent header, user-visible text, org-wide',
        ]:
            self.assertEqual(kinds(text), [], text)

    def test_each_deny_shape_is_refused(self):
        for text, what in [
            ('alice alice 600 hook.sock', 'stat owner'),
            ('-rw------- 1 alice staff 0 Nov  4 hook.sock', 'ls -l owner'),
            ('drwxr-xr-x. 2 vfarcic devs 4096 Nov  4 dir', 'ls -l owner'),
            ('uid=1001(alice) gid=1001(alice)', 'id listing'),
            ('USER=alice', 'user variable'),
            ('cat /home/alice/.codex/auth.json', 'home path'),
            ('/Users/alice/project', 'home path'),
            ('OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuv', 'API key'),
            ('sk-ant-api03-abcdefghijklmnop', 'API key'),
            ('"organization": "org-AbCdEfGhIjKlMnOpQr"', 'OpenAI id'),
            ('user-AbCdEfGhIjKlMnOpQrSt', 'OpenAI id'),
            ('eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sig', 'JWT'),
            ('ghp_' + 'a' * 36, 'GitHub token'),
            ('AKIAABCDEFGHIJKLMNOP', 'AWS key'),
            ('Authorization: Bearer abcdefghijklmnopqrstuvwx', 'bearer token'),
            ('"account_id": "a1b2c3d4-e5f6"', 'secret field'),
            ('mail me at someone@example.com', 'email'),
        ]:
            self.assertIn(what, kinds(text), text)

    def test_styling_inside_a_word_does_not_hide_it(self):
        self.assertIn('stat owner', kinds('al\x1b[1mice\x1b[0m alice\x1b[38;2;1;2;3m 600 x'))
        self.assertIn('home path', kinds('\x1b[2m/ho\x1b[22mme/alice'))

    def test_the_running_user_is_refused_unless_allowed(self):
        self.assertEqual(kinds('drawn by bob in a sentence', users={'bob'}), ['running user'])
        self.assertEqual(kinds('drawn by bobby', users={'bob'}), [])
        self.assertEqual(kinds('vfarcic drew it', users={'vfarcic'}), [])

    def test_the_allow_list_is_what_admits_the_committed_owner_rows(self):
        files = export.committed(str(SMALL_1))
        self.assertEqual(export.scan(files), [])
        refused = export.scan(files, allowed=frozenset())
        self.assertTrue(refused)
        self.assertEqual({(what, excerpt) for _, what, excerpt in refused}, {('stat owner', 'vfar…')})

    def test_export_refuses_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            capture = Path(tmp) / 'capture'
            capture.mkdir()
            (capture / 'trace.bin').write_bytes(b'\x1b[2m  \xe2\x94\x94 cat /home/alice/notes\x1b[0m')
            (capture / 'index.json').write_text('{}')
            (capture / 'result.json').write_text(json.dumps({'hooks_state': None}))
            stem = str(Path(tmp) / 'out')
            self.assertEqual(export.main([str(capture), stem]), 2)
            self.assertEqual(sorted(p.name for p in Path(tmp).iterdir()), ['capture'])
            # The same capture without the path exports, and checks clean.
            (capture / 'trace.bin').write_bytes(b'\x1b[2m  \xe2\x94\x94 cat notes\x1b[0m')
            self.assertEqual(export.main([str(capture), stem]), 0)
            self.assertEqual(export.main(['--check', str(capture), stem]), 0)
            self.assertEqual(export.main(['--scan', stem]), 0)
            # A committed file edited to carry one fails both `--check` and
            # `--scan`, which the fast tier runs over every committed capture.
            Path(stem + '.json').write_text('{"note": "someone@example.com"}')
            self.assertEqual(export.main(['--check', str(capture), stem]), 2)
            self.assertEqual(export.main(['--scan', stem]), 2)

    def test_scan_with_no_stem_is_refused(self):
        self.assertEqual(export.main(['--scan']), 2)


if __name__ == '__main__':
    unittest.main(verbosity=2)
