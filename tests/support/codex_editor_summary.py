#!/usr/bin/env python3
"""Compact, committable summaries of `codex_editor_measure.py` captures
(PRD #1666 part 3, U4), for `tests/fixtures/codex-inbox/release-0.164.0/
editor-probe/`.

    codex_editor_summary.py <capture> <dir>          write the summary (and,
                                                     for `editor` and `e1f`,
                                                     each launch's raw PTY
                                                     bytes) into <dir>
    codex_editor_summary.py --check <capture> <dir>  regenerate and compare
                                                     byte for byte
    codex_editor_summary.py --scan <dir>             the deny scan over the
                                                     committed files

A summary is `<mode>.summary.json`: the capture's `result.json` with the
sandbox root written `<root>`, each row cut to the fields a reading uses
(screens are kept only as the `■` error line), and the recorder's SHA-256.
The raw bytes are `<mode>-<launch>.bin`, unchanged: the deck's own marker
reader replays them (`tests/codex_editor_evidence.rs`).

The deny scan is `codex_head_echo_export.py`'s: an export that would commit
a username, a `/home/` path, a key, token or email refuses and writes nothing.
"""
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import codex_head_echo_export as deny  # noqa: E402

RAW_MODES = ('editor', 'e1f')


def error_line(screen):
    lines = [line.strip() for line in (screen or '').splitlines() if '■' in line]
    return lines[-1] if lines else None


def keep(row, mode):
    """The fields a reading uses."""
    if mode == 'editor':
        start = row.get('start') or {}
        return {k: v for k, v in {
            'cell': row['cell'], 'trial': row.get('trial'), 'label': row['label'], 'launch': row['launch'],
            'env': row.get('env'), 'path_stubs': row.get('path_stubs'), 'draft': row.get('draft'),
            'opened': row['opened'], 'argv_tail': start.get('argv', [])[-2:] if start else None,
            'editor_ran': start['argv'][3] if start else None,
            'visual': start.get('visual'), 'editor': start.get('editor'),
            'isatty': start.get('isatty'), 'export_rule': row.get('export_rule'),
            'export_had_draft': row.get('export_had_draft'), 'rc': (row.get('exit') or {}).get('rc'),
            'composer_after': row['composer_after'], 'error_line': error_line(row['screen_after']),
            'sent_off': row['sent_off'], 'start_off': row.get('start_off'), 'exit_off': row.get('exit_off'),
            'done_off': row['done_off'], 'marks': row['marks']}.items() if v is not None}
    if mode in ('startup', 'config-writes'):
        return row
    if mode == 'e1f':
        return {k: row[k] for k in ('arm', 'trial', 'launch', 'opened', 'sent_off', 'late_off', 'disable_off',
                                     'enable_off', 'frame_off', 'check_off', 'next_export', 'kept', 'marks')}
    if mode == 'keymap':
        out = {k: row.get(k) for k in ('case', 'args', 'cwd', 'files', 'dialog', 'started', 'ctrl_g_opened',
                                       'f6_opened', 'exit_status')}
        if not row.get('started'):
            out['startup_error'] = [line.strip() for line in (row.get('startup_screen') or '').splitlines()
                                    if line.strip()][-4:]
        return out
    if mode == 'forms':
        out = {k: row.get(k) for k in ('cell', 'state', 'form', 'split', 'gap_s', 'opened', 'composer_after')}
        if row['cell'] == 'paste-bel':
            out.update(run=row['run'], next_export_hex=row['next_export_hex'])
        return out
    raise ValueError(mode)


def build(capture):
    capture = Path(capture)
    result = json.loads((capture / 'result.json').read_text())
    root = str(capture.parent)
    mode = result['mode']
    rows = [keep(row, mode) for row in result['rows']]
    summary = {k: result[k] for k in ('release', 'sha256', 'mode', 'error', 'size', 'term', 'colorterm',
                                      'env_names', 'setup', 'recorder_sha256')}
    summary['rows'] = rows
    text = json.dumps(summary, indent=1, ensure_ascii=False).replace(root, 'ROOT') + '\n'
    files = {f'{mode}.summary.json': text.encode()}
    if mode in RAW_MODES:
        for launch in sorted({row['launch'] for row in result['rows']}):
            files[f'{mode}-{launch}.bin'] = (capture / f'{launch}.bin').read_bytes()
    return files


def scan(files):
    findings = deny.scan(files, users=deny.running_users())
    return deny.report(findings)


def main(argv):
    if argv[:1] == ['--scan']:
        target = Path(argv[1])
        files = {p.name: p.read_bytes() for p in sorted(target.iterdir()) if p.is_file() and p.name != 'README.md'}
        return 2 if deny.report(deny.scan(files)) or not files else 0
    check = argv[:1] == ['--check']
    if check:
        argv = argv[1:]
    files = build(argv[0])
    target = Path(argv[1])
    if scan(files):
        return 2
    if check:
        differ = [name for name, data in files.items() if (target / name).read_bytes() != data]
        for name in differ:
            print('differs: ' + name)
        return 1 if differ else 0
    target.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (target / name).write_bytes(data)
        print('wrote ' + str(target / name))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
