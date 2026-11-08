#!/usr/bin/env python3
"""Export Q143b captures into tests/fixtures/claude-inbox/history-recall-submit.

usage: claude_history_submit_export.py [--check] <test-log> <fixture-dir>

The test log is the `--no-capture` output of the
`doorbell_036_live_release_31[78]_history_submit_*` tests; each
`history-submit outcome:` line names its capture. For each trial this writes
`<release>/<case>-x<presses>-<rows>x<cols>-t<trial>.bin` (the raw PTY bytes)
and `.json` (the recorder's index, which the fast tests' `fixture()` loader
reads, plus the outcome, the hook payloads' prompts and the SHA-256 of the
executable, the trace and both recorder files the capture ran with).
`--check` writes nothing and fails unless every file would be written
byte for byte as it is. Reads only the captures; changes nothing under them.
"""
import hashlib
import json
from pathlib import Path
import sys


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    args = sys.argv[1:]
    check = args[:1] == ['--check']
    if check:
        args = args[1:]
    log, target = Path(args[0]), Path(args[1])
    files = {}
    for line in log.read_text(errors='replace').splitlines():
        if 'history-submit outcome:' not in line:
            continue
        outcome = json.loads(line.split('history-submit outcome:', 1)[1])
        capture = Path(outcome['capture'])
        root = capture.parent
        index = json.loads((capture / 'index.json').read_text())
        result = json.loads((capture / 'result.json').read_text())
        trace = (capture / 'trace.bin').read_bytes()
        assert sum(index['read_lens']) == len(trace), capture
        hooks = [json.loads(row)['payload'] for row in
                 (capture / 'hook.jsonl').read_text().splitlines()] if (capture / 'hook.jsonl').exists() else []
        rows, cols = index['rows'], index['cols']
        stem = f"{outcome['release']}/{outcome['case']}-x{outcome['presses']}-{rows}x{cols}-t{outcome['trial']}"
        record = dict(index)
        record.update({
            'release': result['release'],
            'executable_sha256': result['sha256'],
            'recorder': 'tests/support/claude_history_submit_measure.py',
            'recorder_sha256': sha((root / 'history_submit.py').read_bytes()),
            'release_recorder_sha256': sha((root / 'measure.py').read_bytes()),
            'trace_sha256': sha(trace),
            'hook_prompts': [hook.get('prompt') for hook in hooks],
            'hook_events': [hook.get('hook_event_name') for hook in hooks],
            'outcome': outcome,
            'load': result.get('load'),
        })
        files[f'{stem}.bin'] = trace
        files[f'{stem}.json'] = (json.dumps(record, indent=2, ensure_ascii=False) + '\n').encode()
    assert files, 'no outcome lines in the log'
    differ = []
    for name, data in sorted(files.items()):
        path = target / name
        if check:
            if not path.exists() or path.read_bytes() != data:
                differ.append(name)
            continue
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    if differ:
        sys.exit('differ: ' + ', '.join(differ))
    print(f"{'checked' if check else 'wrote'} {len(files)} files")


if __name__ == '__main__':
    main()
