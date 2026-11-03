#!/usr/bin/env python3
"""Compact evidence summaries from a measurement capture's `result.json`.

The files under `tests/fixtures/claude-inbox/release-<release>/` named
`editor-probe-*.json` and `m59-*.json` are written by this script from the
raw capture each one names in `source_capture` (kept under `/var/tmp` on
the recording machine; the PTY bytes stay there). No test runs it: it is
the record of how a summary was made, so a reader can regenerate one from a
capture and see that nothing else went in.

    python3 -I tests/support/claude_evidence_summary.py <capture-dir> <out.json>
    python3 -I tests/support/claude_evidence_summary.py --check <capture-dir> <committed.json>

`--check` exits non-zero when the summary it would write differs from the
committed file byte for byte.

Field mapping (everything not named here is copied unchanged, in order):

Header, every kind: `release`, `sha256`, `recorder_sha256` and `error` from
`result.json`; `source_capture` is the capture directory's absolute path.

Editor probe (`claude_editor_measure.py`, a `rows` list):
  - mode `editor` (export, selection, toggles): each row drops `start`,
    `exit` and `screen_after`, and gains, from the editor stand-in's
    `start` record, `editor_argv` (its argv), `export_path` (`path`) and
    `export_contents` (`contents_hex` decoded as UTF-8).
  - modes `keymap-*`: each press record nested in a row (a dict carrying
    `key_hex`) becomes `{"opened": <its opened>}`; every other field,
    `samples` lists included, is copied.
  - mode `forms` (no `rows` in the summary): `opened_of_cells` maps
    `<state>/<form>` to `"<opened>/<presses>"` over the rows whose `cell` is
    `forms`, keys sorted; `esc_bel_gap` is `{gap_s, opened}` per
    `esc-bel-gap` row in recording order; `paste_bel` is
    `{split, opened, next_export}` per `paste-bel` row, `next_export` being
    `next_export_hex` (the bytes the editor stand-in read from the next
    export file) decoded as UTF-8. It is not `composer_after`, which is the
    screen text: a BEL kept in the draft is not drawn, so the screen reads
    `BASEPASTETAIL` either way.

M59 (`claude_helper_alarm_measure.py`, `trials` becomes `rows`): each trial
drops `trace`, `editor` and `screen_after`; gains `trace_events`, the
trace's event strings without its `poll`, `polled`, `send` and `sent`
lines, and `editor_stages`, `[stage, rc]` per editor stand-in record.
"""
import json
import sys
from pathlib import Path

DROPPED_TRACE = ('poll ', 'polled ', 'send ', 'sent ')


def header(capture, result):
    return {
        'release': result['release'],
        'sha256': result['sha256'],
        'source_capture': str(capture),
        'recorder_sha256': result['recorder_sha256'],
        'error': result['error'],
    }


def press(value):
    if isinstance(value, dict) and 'key_hex' in value:
        return {'opened': value['opened']}
    return value


def editor_rows(result):
    rows = []
    for row in result['rows']:
        start = row.get('start') or {}
        out = {k: v for k, v in row.items() if k not in ('start', 'exit', 'screen_after')}
        out['editor_argv'] = start.get('argv')
        out['export_path'] = start.get('path')
        hexed = start.get('contents_hex')
        out['export_contents'] = bytes.fromhex(hexed).decode() if hexed is not None else None
        rows.append(out)
    return rows


def forms(result):
    cells = {}
    gaps, pastes = [], []
    for row in result['rows']:
        if row['cell'] == 'forms':
            key = f"{row['state']}/{row['form']}"
            opened, presses = cells.get(key, (0, 0))
            cells[key] = (opened + bool(row['opened']), presses + 1)
            if row['form'] == 'esc-bel-gap':
                gaps.append({'gap_s': row['gap_s'], 'opened': row['opened']})
        elif row['cell'] == 'paste-bel':
            pastes.append({'split': row['split'], 'opened': row['opened'],
                           'next_export': bytes.fromhex(row['next_export_hex']).decode()})
    return {
        'opened_of_cells': {k: f'{o}/{n}' for k, (o, n) in sorted(cells.items())},
        'esc_bel_gap': gaps,
        'paste_bel': pastes,
    }


def m59_rows(result):
    rows = []
    for trial in result['trials']:
        out = {k: v for k, v in trial.items() if k not in ('trace', 'editor', 'screen_after')}
        out['trace_events'] = [e['event'] for e in trial['trace']
                               if not e['event'].startswith(DROPPED_TRACE)]
        out['editor_stages'] = [[e.get('stage'), e.get('rc')] for e in trial['editor']]
        rows.append(out)
    return rows


def summarize(capture):
    capture = Path(capture).resolve()
    result = json.loads((capture / 'result.json').read_text())
    summary = header(capture, result)
    if 'trials' in result:
        summary['rows'] = m59_rows(result)
    elif result.get('mode') == 'forms':
        summary.update(forms(result))
    elif result.get('mode') == 'editor':
        summary['rows'] = editor_rows(result)
    else:
        summary['rows'] = [{k: press(v) for k, v in row.items()} for row in result['rows']]
    return json.dumps(summary, indent=1) + '\n'


def main(argv):
    check = argv[:1] == ['--check']
    capture, target = argv[1:] if check else argv
    text = summarize(capture)
    if check:
        same = Path(target).read_text() == text
        print(f"{'same' if same else 'DIFFERS'}: {target}")
        return 0 if same else 1
    Path(target).write_text(text)
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
