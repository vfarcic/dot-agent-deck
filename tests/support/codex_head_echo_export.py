#!/usr/bin/env python3
"""Export one Codex `head_echo` capture (`codex_head_echo_measure.py`) as a
committed fixture, or check a committed one against its capture.

Usage: codex_head_echo_export.py [--check] CAPTURE_DIR STEM

Writes STEM.bin (the raw PTY bytes, `trace.bin`), STEM.json (`index.json`
as recorded) and STEM.result.json: `result.json` with Codex's `config.toml`
text replaced by what it says about the deck's hooks (each hook state entry's
`enabled` and whether it carries a trusted hash). Nothing else is changed or dropped. `--check` writes nothing and
exits non-zero when any of the three would differ.
"""
import json
import sys
import tomllib
from pathlib import Path


def hooks_summary(config_text):
    if config_text is None:
        return None
    state = tomllib.loads(config_text).get('hooks', {}).get('state', {})
    return {key: {'enabled': entry.get('enabled', True), 'trusted': 'trusted_hash' in entry}
            for key, entry in sorted(state.items())}


def exported(capture):
    result = json.loads((capture / 'result.json').read_text())
    result['hooks'] = hooks_summary(result.pop('hooks_state', None))
    return {
        '.bin': (capture / 'trace.bin').read_bytes(),
        '.json': (capture / 'index.json').read_bytes(),
        '.result.json': (json.dumps(result, indent=2, ensure_ascii=False) + '\n').encode(),
    }


def main(argv):
    check = argv[:1] == ['--check']
    if check:
        argv = argv[1:]
    capture, stem = Path(argv[0]), argv[1]
    differs = False
    for ext, data in exported(capture).items():
        target = Path(stem + ext)
        if check:
            if not target.exists() or target.read_bytes() != data:
                print('differs: %s' % target)
                differs = True
        else:
            target.write_bytes(data)
    return 1 if differs else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
