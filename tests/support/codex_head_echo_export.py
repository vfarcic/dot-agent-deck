#!/usr/bin/env python3
"""Export one Codex `head_echo` capture (`codex_head_echo_measure.py`) as a
committed fixture, check a committed one against its capture, or scan
committed ones for what must never be committed.

Usage: codex_head_echo_export.py [--check] CAPTURE_DIR STEM
       codex_head_echo_export.py --scan STEM...

Writes STEM.bin (the raw PTY bytes, `trace.bin`), STEM.json (`index.json`
as recorded) and STEM.result.json: `result.json` with Codex's `config.toml`
text replaced by what it says about the deck's hooks (each hook state entry's
`enabled` and whether it carries a trusted hash). Nothing else is changed or
dropped. `--check` writes nothing and exits non-zero when any of the three
would differ, or when the committed files or the export fail the deny scan.

The deny scan. Every submitted line starts a real model turn, and whatever
the model's tools print is drawn in Codex's transcript and so lands in the
PTY bytes (one committed 24 x 80 capture draws a `stat` of the deck's sockets,
owner and group included). So the export refuses, writing nothing, when the
three files would contain:
- an OS username other than the allow-listed owner handle: the user running
  the export, and any name in an owner/group column (`ls -l`, `stat -c '%U %G
  %a'`), an `id` listing (`uid=…(name)`) or a `USER=`/`LOGNAME=` assignment;
- a `/home/…` or `/Users/…` path (one not inside another path);
- an API key, token or JWT shape, an OpenAI `org-`/`user-`/`proj-` id, a
  secret-named field with a value, or an email address.
`--scan` applies the same scan, apart from the running user's name (which
depends on who runs it), to committed fixtures: the fast tier runs it over
every committed capture. The scan reads the bytes with styling removed and
every other escape sequence as a break; text a terminal draws in pieces
across rows can still evade it, so it is a net, not a review.
"""
import json
import os
import pwd
import re
import sys
from pathlib import Path

# The repository owner's public GitHub handle, which is also the recording
# machine's OS username (it appears throughout the repository already).
ALLOWED_USERS = frozenset({'vfarcic'})
# Accounts that name no person.
SYSTEM_ACCOUNTS = frozenset({'root', 'nobody', 'nogroup'})

NAME = r'[A-Za-z_][A-Za-z0-9_.-]{0,31}'
# Patterns whose `u` (and `g`) groups are usernames, checked against the
# allow-list.
USER_PATTERNS = [
    ('ls -l owner', re.compile(r'(?<![\w-])[-bcdlpsD][-rwxsStT]{9}[.+@]?\s+\d+\s+(?P<u>%s)\s+(?P<g>%s)\s+\d'
                               % (NAME, NAME))),
    ('stat owner', re.compile(r'(?<![\w.-])(?P<u>%s)\s+(?P=u)\s+[0-7]{3,4}(?![\w.-])' % NAME)),
    ('id listing', re.compile(r'\b[ug]id=\d+\((?P<u>[^)\s]{1,32})\)')),
    ('user variable', re.compile(r'\b(?:USER|LOGNAME|USERNAME)=(?P<u>%s)' % NAME)),
]
# Patterns that are refused whatever they hold.
DENY_PATTERNS = [
    ('home path', re.compile(r'(?<![\w.~/-])/(?:home|Users)/[^\s/"\'\\]+')),
    ('API key', re.compile(r'(?<![A-Za-z0-9])sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}')),
    ('OpenAI id', re.compile(r'(?<![A-Za-z0-9_-])(?:org|user|proj|acct)-[A-Za-z0-9]{16,}')),
    ('JWT', re.compile(r'eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.')),
    ('GitHub token', re.compile(r'(?<![A-Za-z0-9])(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})')),
    ('AWS key', re.compile(r'(?<![A-Z0-9])(?:AKIA|ASIA)[0-9A-Z]{16}(?![A-Z0-9])')),
    ('Slack token', re.compile(r'(?<![A-Za-z0-9])xox[abprs]-[A-Za-z0-9-]{10,}')),
    ('bearer token', re.compile(r'(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}')),
    ('secret field', re.compile(
        r'(?i)\b(?:api[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|auth[_-]?token'
        r'|account[_-]?id|client[_-]?secret|password|passwd)["\']?\s*[:=]\s*["\']?[^\s"\',}]{6,}')),
    ('email', re.compile(r'[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}')),
]

# Styling splits a drawn word without breaking it; any other escape sequence
# (cursor moves, OSC, charset) is read as a break.
SGR = re.compile(r'\x1b\[[0-9;:]*m')
ESCAPE = re.compile(r'\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[()*+][0-9A-Za-z]|[ -/]*[0-~])')


def readable(data):
    return ESCAPE.sub('\n', SGR.sub('', data.decode('utf-8', 'replace')))


def running_users():
    """The OS username(s) of whoever runs the export."""
    names = {pwd.getpwuid(os.getuid()).pw_name}
    names.update(os.environ.get(v) for v in ('USER', 'LOGNAME') if os.environ.get(v))
    return names


def masked(text):
    return text[:4] + '…' if len(text) > 4 else text


def scan(files, allowed=ALLOWED_USERS, users=()):
    """Findings, as `(file, what, masked excerpt)`, in `files` (name ->
    bytes). `users` are further usernames refused wherever they appear as a
    word."""
    found = []
    named = [re.compile(r'(?<![A-Za-z0-9_.-])%s(?![A-Za-z0-9_-])' % re.escape(u))
             for u in sorted(set(users) - allowed - SYSTEM_ACCOUNTS)]
    for name, data in files.items():
        text = readable(data)
        for what, pattern in USER_PATTERNS:
            for m in pattern.finditer(text):
                for user in {m.group(g) for g in ('u', 'g') if g in pattern.groupindex}:
                    if user not in allowed and user not in SYSTEM_ACCOUNTS:
                        found.append((name, what, masked(user)))
        for what, pattern in DENY_PATTERNS:
            for m in pattern.finditer(text):
                found.append((name, what, masked(m.group())))
        for pattern in named:
            for m in pattern.finditer(text):
                found.append((name, 'running user', masked(m.group())))
    return found


def hooks_summary(config_text):
    if config_text is None:
        return None
    import tomllib
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


def committed(stem):
    return {stem + ext: Path(stem + ext).read_bytes() for ext in ('.bin', '.json', '.result.json')}


def report(findings):
    for name, what, excerpt in findings:
        print('refused: %s: %s %r' % (name, what, excerpt))
    return bool(findings)


def main(argv):
    if argv[:1] == ['--scan']:
        refused = False
        for stem in argv[1:]:
            refused |= report(scan(committed(stem)))
        return 2 if refused or len(argv) < 2 else 0
    check = argv[:1] == ['--check']
    if check:
        argv = argv[1:]
    capture, stem = Path(argv[0]), argv[1]
    data = exported(capture)
    refused = report(scan({stem + ext: b for ext, b in data.items()}, users=running_users()))
    if not check:
        if refused:
            return 2
        for ext, b in data.items():
            Path(stem + ext).write_bytes(b)
        return 0
    differs = False
    for ext, b in data.items():
        target = Path(stem + ext)
        if not target.exists() or target.read_bytes() != b:
            print('differs: %s' % target)
            differs = True
    if all(Path(stem + ext).exists() for ext in data):
        refused |= report(scan(committed(stem), users=running_users()))
    return 2 if refused else 1 if differs else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
