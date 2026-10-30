"""Opt-in external-editor behavior shared by the synthetic composer screens.

Copy this file beside a copied composer when enabling EXTERNAL_EDIT. See
composer-external-edit.md for the measured sources and deliberate limitations.
Nothing in this module is loaded by the default-off path.
"""
import json
import os
from pathlib import Path
import secrets
import select
import shlex
import shutil
import string
import subprocess
import termios
import time
import tty
import uuid


class ExternalEdit:
    def __init__(self, base, agent, terminal):
        self.base, self.agent, self.terminal = base, agent, terminal
        self.escape = bytearray()
        self.paste = False
        self.error = b''
        # M44e's ordinary-composer samples, not the production detector's
        # conservative candidate union. The empty stand-in copies Claude as
        # a synthetic convention; other UI states/releases are not modeled.
        self.shortcuts = ({b'\x1b[103;5u', b'\x1b[103;5:1u', b'\x1b[103;5:2u'}
                          if agent == 'codex' else
                          {b'\x1b[103;5u', b'\x1b[71;5u',
                           b'\x1b[27;5;103~', b'\x1b[27;5;71~'})
        os.write(1, b'\x1b[?2004h')

    def path(self, suffix):
        return Path(self.base + suffix)

    def tokens(self, chunk):
        """Yield ordinary bytes, literal paste bytes, or None for a Ctrl+G."""
        for value in chunk:
            if self.escape:
                self.escape.append(value)
                if len(self.escape) == 2 and value != 91:
                    self.escape.clear()
                elif len(self.escape) >= 3 and 0x40 <= value <= 0x7e:
                    sequence = bytes(self.escape)
                    self.escape.clear()
                    if sequence == b'\x1b[200~':
                        self.paste = True
                    elif sequence == b'\x1b[201~':
                        self.paste = False
                    elif not self.paste and sequence in self.shortcuts:
                        yield None
                continue
            if value == 27:
                self.escape.append(value)
            elif self.paste:
                # M44f: Claude retains a pasted BEL; Codex removes it.
                if value != 7 or self.agent != 'codex':
                    yield bytes([value])
            elif value == 7:
                yield None
            else:
                yield value

    def command(self, path):
        if self.agent == 'codex':
            # M34: presence, not truthiness; shell-word grouping, no shell.
            value = os.environ.get('VISUAL', os.environ.get('EDITOR'))
            if value is None:
                raise ValueError('Cannot open external editor: set $VISUAL or $EDITOR before starting Codex')
            command = shlex.split(value)
            if not command:
                raise ValueError('empty editor command')
            return [*command, str(path)]
        # M8, M20/M22/M23 and q35pre: truthy fallback and literal spaces.
        value = os.environ.get('VISUAL') or os.environ.get('EDITOR')
        if not value:
            value = next((name for name in ('code', 'vi', 'nano') if shutil.which(name)), None)
        if value is None:
            raise ValueError('synthetic fixture: no editor executable')
        value = {'code': 'code -w', 'subl': 'subl --wait'}.get(value, value)
        command = [word for word in value.split(' ') if word]
        name = os.path.basename(command[0])
        if any(editor in name for editor in ('code', 'cursor', 'windsurf', 'codium')):
            return [*command, '-g', str(path) + ':1']
        if 'subl' in name:
            return [*command, str(path) + ':1']
        if name in ('vi', 'vim', 'nvim', 'nano', 'emacs', 'pico', 'micro', 'helix', 'hx'):
            return [*command, '+1', str(path)]
        return [*command, str(path)]

    def export(self, draft):
        if self.agent == 'codex':
            directory = Path(os.environ.get('CODEX_HOME', str(Path.home() / '.codex'))) / 'editor'
            directory.mkdir(parents=True, exist_ok=True)
            name = '.tmp' + ''.join(secrets.choice(string.ascii_letters + string.digits) for _ in range(6)) + '.md'
            mode = 0o600
        else:
            # M20: directory 0700, file 0666 filtered by the inherited umask.
            directory = Path(os.environ.get('TMPDIR', '/tmp')) / f'claude-{os.getuid()}'
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            name, mode = f'claude-prompt-{uuid.uuid4()}.md', 0o666
        path = directory / name
        with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode), 'wb') as output:
            output.write(draft)
        return path

    def invoke(self, draft):
        """Return whether to discard this read's suffix; edit draft in place."""
        if self.path('.edit-drop').exists():
            return False
        try:
            self.path('.edit-ignore-next').unlink()
        except FileNotFoundError:
            pass
        else:
            return False
        try:
            mechanism = self.path('.edit-mechanism').read_text().strip()
        except FileNotFoundError:
            mechanism = 'none'
        self.path('.edit-dispatched').write_text(str(time.monotonic_ns()))
        delay = 0.5 if self.path('.edit-delay').exists() or mechanism == 'H-ignore' else 0
        deadline = time.monotonic() + delay
        while time.monotonic() < deadline:
            remaining = max(0, deadline - time.monotonic())
            if mechanism == 'H-ignore':
                if select.select([0], [], [], min(0.01, remaining))[0]:
                    os.read(0, 4096)
            else:
                time.sleep(min(0.01, remaining))
        path = self.export(bytes(draft))
        disables = 2 if self.agent == 'codex' else 1
        os.write(1, b'\x1b[?2004l' * disables)
        self.error = b''
        try:
            command = self.command(path)
            # q35pre's measured editor stdin is canonical and echoing. TCSANOW
            # deliberately keeps queued bytes unless H-flush asks to lose them.
            termios.tcsetattr(0, termios.TCSANOW, self.terminal)
            if mechanism == 'H-flush':
                termios.tcflush(0, termios.TCIFLUSH)
            result = subprocess.run(command)
            if self.agent == 'codex':
                # M31: keys unread by an editor during suspension disappear.
                termios.tcflush(0, termios.TCIFLUSH)
            if result.returncode == 0:
                edited = path.read_bytes()
                draft[:] = edited.rstrip() if self.agent == 'codex' else edited
            elif self.agent == 'codex':
                status = (f'exit status: {result.returncode}' if result.returncode > 0
                          else f'signal: {-result.returncode}')
                self.error = ('Failed to open editor: editor exited with status ' + status).encode()
        except (OSError, ValueError) as error:
            self.error = str(error).encode()
        finally:
            tty.setraw(0, termios.TCSANOW)
            path.unlink(missing_ok=True)
            os.write(1, b'\x1b[?2004h')
        # The M31 suspended-interval flush above is separate from a suffix
        # already in this read. M44d's sampled Codex same-frame BEL controls
        # retain that suffix; dropping it is an explicit synthetic injection.
        return mechanism == 'H-chunk'
