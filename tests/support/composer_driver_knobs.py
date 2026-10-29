"""Default-off SC-06 output faults, shared by the two real-PTY stand-ins.

These modify bytes, never report a verdict. A single redraw remains a single
os.write, including an overlay drawn and erased inside that write.
"""
import os
import termios

PREFIX = 'DOT_AGENT_DECK_TEST_COMPOSER_'


def input_mode(fd):
    if os.environ.get(PREFIX + 'DRIVER_FAULT') == 'echo_mode':
        attrs = termios.tcgetattr(fd)
        attrs[3] |= termios.ECHO
        termios.tcsetattr(fd, termios.TCSANOW, attrs)


def visible_composer(composer):
    if os.environ.get(PREFIX + 'DRIVER_FAULT') == 'missing_echo':
        return b''
    return composer


def frame(data, agent, redraw_number):
    fault = os.environ.get(PREFIX + 'DRIVER_FAULT', '')
    # Keep the default path byte-identical, including the number of writes.
    if not fault and os.environ.get(PREFIX + 'TIMING_RESTORE') != '1':
        return data
    end = b'\x1b[?2026l' if agent == 'codex' else b''
    body = data[:-len(end)] if end else data
    cursor = body[body.rfind(b'\x1b['):]
    if fault == 'overlay':
        body += b'\x1b[35;5H!\x1b[2K'
    elif fault == 'moved_glyph':
        body += b'\x1b[35;5HC\x1b[2K'
    elif fault == 'mode':
        body += b'\x1b[?1000h\x1b[?1000l'
    elif fault == 'control':
        body += b'\x1b]52;c;YQ==\x07'
    elif fault == 'modal':
        body += b'\x1b[35;5HConfirm?'
    elif fault == 'missing_echo' or fault == 'echo_mode':
        return data
    elif fault:
        raise ValueError('unknown DRIVER_FAULT: ' + fault)
    # Restore the reviewed frame's cursor after the injected operations.
    if fault in ('overlay', 'moved_glyph', 'mode', 'control'):
        body += cursor
    if os.environ.get(PREFIX + 'TIMING_RESTORE') == '1' and redraw_number > 1:
        if agent != 'claude':
            raise ValueError('TIMING_RESTORE is a Claude quiet-gap control')
        # At the placeholder's own cells: erase Tr, restore Tr, then draw
        # the typed composer. The restored Tr belongs only to the old frame.
        body = b'\x1b[26;3H\x1b[2X\x1b[H\x1b[26;3H\x1b[2mTr\x1b[22m\x1b[H' + body
    return body + end
