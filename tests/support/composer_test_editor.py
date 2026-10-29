#!/usr/bin/env python3
"""Trusted synthetic editor: MODE BASE [literal arguments ...] EXPORT.

Modes: fast (no intentional wait), fail (edit then exit 1), trim (write trailing
whitespace), hang/wait (wait for BASE.editor-go), read (read stdin, then wait),
late-hang (wait for BASE.editor-publish-go before publishing, then hang).
BASE.editor-started atomically records argv, original bytes, isatty booleans and pid.
This is a test fixture, not an editor sandbox. See composer-external-edit.md.
"""
import json
import os
from pathlib import Path
import sys
import time

mode, base, *args = sys.argv[1:]
path = Path(args[-1].removesuffix(':1'))
record = {'argv': args, 'path': str(path), 'hex': path.read_bytes().hex(),
          'mode': path.stat().st_mode & 0o777, 'tty': [os.isatty(n) for n in range(3)],
          'pid': os.getpid()}
if mode == 'late-hang':
    Path(base + '.editor-publish-waiting').touch()
    while not os.path.exists(base + '.editor-publish-go'):
        time.sleep(0.005)
temporary = Path(base + '.editor-started.tmp')
temporary.write_text(json.dumps(record))
temporary.replace(base + '.editor-started')
if mode in ('wait', 'hang', 'read', 'late-hang'):
    if mode == 'read':
        data = os.read(0, 1024)
        Path(base + '.editor-input').write_bytes(data)
    while not os.path.exists(base + '.editor-go'):
        time.sleep(0.005)
path.write_bytes(b'EDITED   \n' if mode == 'trim' else b'EDITED')
Path(base + '.editor-finished').write_text('finished')
sys.exit(1 if mode == 'fail' else 0)
