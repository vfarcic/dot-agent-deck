#!/usr/bin/env python3
"""Copy one `claude_release_measure.py` capture into a committed fixture.

Not run by any test: whoever records a release runs it by hand, once per
capture chosen for the fixture set, from the checkout:

    python3 -I tests/support/claude_fixture_export.py <kind> <capture> <stem>

`<capture>` is the `capture` directory a lane-2 test printed, under
`/var/tmp/dad-claude-release-measure-*/`; `<stem>` is the fixture path
without its extension, e.g.
`tests/fixtures/claude-inbox/r5wake/claude-2.1.318-start-50x200`.

What it writes, by kind:

- `plain` (start-up, composer, layout): `<stem>.bin` is the capture's
  `trace.bin` unchanged and `<stem>.json` its `index.json` unchanged, as the
  2.1.310, 2.1.312 and 2.1.317 start-up, composer and layout fixtures are.
- `keys` (compound keys): as `plain`, plus an `end` point at the trace's
  length, which the 2.1.312 and 2.1.317 compound-key fixtures carry.
- `menus`, `history`: `<stem>.bin` unchanged; `<stem>.json` is `index.json`
  plus `source_capture` (the capture root's name), `release`,
  `executable_sha256`, `theme`, `colorterm`, `term`, `model` and `events`
  from `result.json`, `trace_sha256` (of `trace.bin`), `recorder_sha256`
  (of the `measure.py` copy the test wrote beside the capture, the recorder
  that made it), `captured_on` (`--captured-on`, default today, UTC),
  `blocked_submissions` (lines of `hook.jsonl`) and `review_status`
  `candidate`, the fields the menu fixtures carry; and `<stem>.frames.txt`
  is the capture's `rendered.txt`, the frames the Rust test replayed through
  vt100, with trailing whitespace trimmed on every line.

It refuses a capture whose `result.json` records an error, and one whose
`trace.bin` length differs from the sum of its read lengths. `--check`
compares instead of writing, and exits non-zero on any difference.
"""
import argparse
import datetime
import hashlib
import json
import sys
from pathlib import Path


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build(kind, capture, captured_on):
    capture = Path(capture)
    result = json.loads((capture / 'result.json').read_text())
    if result.get('error') is not None:
        raise SystemExit(f'{capture}: the capture recorded an error: {result["error"]}')
    index = json.loads((capture / 'index.json').read_text())
    trace = (capture / 'trace.bin').read_bytes()
    if sum(index['read_lens']) != len(trace):
        raise SystemExit(f'{capture}: read lengths do not cover trace.bin')
    out = {'.bin': trace}
    if kind in ('plain', 'keys'):
        if kind == 'keys':
            index['points']['end'] = len(trace)
        out['.json'] = (json.dumps(index, indent=2) + '\n').encode() if kind == 'keys' \
            else (capture / 'index.json').read_bytes()
        return out
    hooks = capture / 'hook.jsonl'
    index.update({
        'source_capture': capture.parent.name,
        'release': result['release'],
        'executable_sha256': result['sha256'],
        'trace_sha256': hashlib.sha256(trace).hexdigest(),
        'theme': result['theme'],
        'colorterm': result['colorterm'],
        'term': result['term'],
        'recorder_sha256': sha256(capture.parent / 'measure.py'),
        'captured_on': captured_on,
        'blocked_submissions': len(hooks.read_text().splitlines()) if hooks.exists() else 0,
        'review_status': 'candidate',
        'model': result['model'],
        'events': result['events'],
    })
    out['.json'] = (json.dumps(index, indent=2, ensure_ascii=False) + '\n').encode()
    rendered = (capture / 'rendered.txt').read_text()
    out['.frames.txt'] = ('\n'.join(line.rstrip() for line in rendered.split('\n'))).encode()
    return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('kind', choices=['plain', 'keys', 'menus', 'history'])
    parser.add_argument('capture')
    parser.add_argument('stem')
    parser.add_argument('--captured-on', default=datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    files = build(args.kind, args.capture, args.captured_on)
    differs = False
    for suffix, data in files.items():
        path = Path(args.stem + suffix)
        if args.check:
            if not path.exists() or path.read_bytes() != data:
                print(f'{path}: differs from {args.capture}', file=sys.stderr)
                differs = True
        else:
            path.write_bytes(data)
            print(f'{path}: {len(data)} bytes')
    sys.exit(1 if differs else 0)


if __name__ == '__main__':
    main()
