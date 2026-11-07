"""Black-box Q35-12 checks; all processes, terminals and paths are fixture-owned.

No e2e helper seam is invented here. Fault-injected SpawnFailed/WaitUnknown,
post-editor pause and syscall tracing remain a separate acceptance obligation.
"""
import errno
import atexit
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import termios
import time

CASE, HELPER, ROOT, SOCKET = sys.argv[1:]
ROOT = Path(ROOT)
NOTICE = (b"dot-agent-deck: the deck was checking this input box. "
          b"If you pressed Ctrl+G, press it again in a moment.\n")
ENV = dict(os.environ, DOT_AGENT_DECK_EDITOR_GEN="3",
           DOT_AGENT_DECK_SOCKET=SOCKET,
           DOT_AGENT_DECK_ATTACH_SOCKET=str(ROOT / "unused.sock"),
           DOT_AGENT_DECK_PANE_ID="fixture", DOT_AGENT_DECK_TOKEN="fixture",
           CODEX_HOME=str(ROOT / "codex"), XDG_CONFIG_HOME=str(ROOT / "config"),
           XDG_STATE_HOME=str(ROOT / "state"), XDG_RUNTIME_DIR=str(ROOT / "runtime"))
EXPORT = ROOT / "codex/editor/.tmpfixture.md"
EXPORT.parent.mkdir(parents=True)
EXPORT.write_text("")
SENTINEL = ROOT / "editor-finished"
EDITOR = ROOT / "sentinel-editor"
EDITOR.write_text("#!/usr/bin/python3\n" + "\n".join([
    "import os, pathlib, signal, sys, time",
    f"assert sys.argv[1:] == [{str(EXPORT)!r}], sys.argv",
    "assert os.environ['VISUAL'] == os.environ['EDITOR']",
    f"assert os.environ['VISUAL'] == {str(EDITOR)!r}",
    "if os.environ['CHECK_CASE'] == 'editor-signal': os.kill(os.getpid(), signal.SIGTERM)",
    "if os.environ['CHECK_CASE'] == 'editor-long': time.sleep(8)",
    f"pathlib.Path({str(SENTINEL)!r}).write_text('finished')",
    "sys.exit(37)", ""]))
EDITOR.chmod(0o700)
ENV.update(CHECK_CASE=CASE, VISUAL=HELPER, EDITOR=HELPER,
           DOT_AGENT_DECK_EDITOR_USER_VISUAL=str(EDITOR),
           DOT_AGENT_DECK_EDITOR_USER_EDITOR=str(EDITOR))
CHILDREN = []


@atexit.register
def cleanup():
    for child in CHILDREN:
        if child.poll() is None:
            child.kill()
            child.wait()


def finish(proc, timeout=7):
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        raise AssertionError(f"{CASE}: helper exceeded {timeout}s; pid={proc.pid}")
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def launch(stderr, preexec=None):
    child = subprocess.Popen([HELPER, str(EXPORT)], env=ENV, cwd=ROOT,
                             stdin=stderr if stderr >= 0 else subprocess.DEVNULL,
                             stdout=stderr,
                             stderr=stderr, preexec_fn=preexec)
    CHILDREN.append(child)
    return child


def read_master(fd):
    result = b""
    while select.select([fd], [], [], 0.05)[0]:
        try:
            chunk = os.read(fd, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                break
            raise
        if not chunk:
            break
        result += chunk
    return result


def terminal(mode, postprocess=True):
    master, slave = pty.openpty()
    attrs = termios.tcgetattr(slave)
    attrs[1] = (attrs[1] | termios.OPOST | termios.ONLCR) if postprocess else 0
    attrs[3] &= ~(termios.ECHO | termios.ICANON)
    termios.tcsetattr(slave, termios.TCSANOW, attrs)
    prefix = b""
    if mode == "stopped":
        termios.tcflow(slave, termios.TCOOFF)
    if mode == "full":
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        fcntl.fcntl(slave, fcntl.F_SETFL, flags | os.O_NONBLOCK)
        full_observations = 0
        while full_observations < 3:
            try:
                prefix += b"P" * os.write(slave, b"P" * 4096)
                full_observations = 0
            except BlockingIOError:
                full_observations += 1
                time.sleep(0.02)
        fcntl.fcntl(slave, fcntl.F_SETFL, flags)
        assert prefix, "full-terminal control must queue bytes"
    return master, slave, prefix


def ignored_alarm():
    signal.signal(signal.SIGALRM, signal.SIG_IGN)
    signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM})


def pty_case_inner(armed, mode, postprocess):
    master, slave, prefix = terminal(mode, postprocess)
    try:
        # The supervisor, rather than the helper, owns the controlling tty.
        # It stays alive through capture, so helper exit cannot hang it up.
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(slave, os.getpgrp())
        proc = launch(slave, ignored_alarm if armed else None)
        rc = finish(proc)
        if mode == "stopped":
            termios.tcflow(slave, termios.TCOON)
        capture = read_master(master)
        print(json.dumps(dict(case=CASE, mode=mode, rc=rc, bytes=len(capture),
                              prefix=len(prefix))), flush=True)
        if armed:
            if mode in ("stopped", "full"):
                assert rc == -signal.SIGALRM, ("armed notice alarm", rc, capture[:256])
                assert capture == prefix, ("helper added bytes to stalled output", capture)
            else:
                expected = NOTICE.replace(b"\n", b"\r\n") if postprocess else NOTICE
                assert capture == expected, ("exact raw master notice", expected, capture)
        else:
            assert rc == 37, ("editor status", rc, capture[:256])
            assert capture == prefix, ("post-editor helper terminal bytes", capture)
        assert SENTINEL.exists() is (not armed), "editor invocation must follow the arm branch"
    finally:
        # Closing the master hangs up the fixture foreground group, including
        # this supervisor. The helper has been reaped; ignore cleanup's HUP.
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        os.close(slave)
        os.close(master)


def pty_case(armed, mode, postprocess=True):
    supervisor = os.fork()
    if supervisor == 0:
        try:
            pty_case_inner(armed, mode, postprocess)
            os._exit(0)
        except BaseException:
            import traceback
            traceback.print_exc()
            os._exit(1)
    _, status = os.waitpid(supervisor, 0)
    assert os.waitstatus_to_exitcode(status) == 0, "foreground terminal scenario failed"


def background(armed):
    # A supervisor owns the terminal/session. Keeping it alive keeps the
    # helper's background group non-orphaned, so SIGTTOU really can stop it.
    supervisor = os.fork()
    if supervisor == 0:
        try:
            master, slave, _ = terminal("normal")
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(slave, os.getpgrp())
            attrs = termios.tcgetattr(slave)
            attrs[3] |= termios.TOSTOP
            termios.tcsetattr(slave, termios.TCSANOW, attrs)
            def child_setup():
                os.setpgid(0, 0)
                signal.signal(signal.SIGTTOU, signal.SIG_DFL)
                signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTTOU})
            proc = launch(slave, child_setup)
            try:
                deadline = time.monotonic() + 8
                stopped = False
                while time.monotonic() < deadline:
                    pid, status = os.waitpid(proc.pid, os.WNOHANG | os.WUNTRACED)
                    if pid:
                        if os.WIFSTOPPED(status):
                            assert armed, ("post-editor helper stopped", os.WSTOPSIG(status))
                            assert os.WSTOPSIG(status) == signal.SIGTTOU, status
                            stopped = True
                            # No daemon E2 exists in this standalone fixture.
                            # The suspended alarm is not a wall-clock exit bound.
                            time.sleep(5.2)
                            assert os.waitpid(proc.pid, os.WNOHANG)[0] == 0
                            os.kill(proc.pid, signal.SIGCONT)
                            assert finish(proc, 2) == -signal.SIGALRM
                            break
                        rc = os.waitstatus_to_exitcode(status)
                        proc.returncode = rc
                        assert not armed, ("armed notice never stopped", rc)
                        assert rc == 37, ("background editor status", rc)
                        break
                    time.sleep(0.01)
                else:
                    raise AssertionError("background helper did not stop or exit")
                assert stopped is armed
                assert read_master(master) == b"", "background path emitted terminal bytes"
            finally:
                if proc.returncode is None:
                    proc.kill()
                    proc.wait()
            os._exit(0)
        except BaseException:
            import traceback
            traceback.print_exc()
            os._exit(1)
    _, status = os.waitpid(supervisor, 0)
    assert os.waitstatus_to_exitcode(status) == 0, "background terminal scenario failed"


if CASE == "armed-pipe":
    proc = launch(subprocess.PIPE, ignored_alarm)
    rc = finish(proc)
    capture = proc.stderr.read()
    assert proc.stdout.read() == b"", "helper stdout must be silent"
    assert capture == NOTICE, ("exact syscall notice bytes", NOTICE, capture, rc)
    assert not SENTINEL.exists(), "armed helper opened the editor"
elif CASE == "armed-pty":
    pty_case(True, "normal")
    pty_case(True, "normal", False)
elif CASE.startswith("armed-") and CASE != "armed-background":
    pty_case(True, CASE.split("-")[1])
elif CASE.endswith("background"):
    background(CASE.startswith("armed"))
elif CASE == "editor-stalled":
    pty_case(False, "stopped")
    SENTINEL.unlink()
    pty_case(False, "full")
else:
    proc = launch(subprocess.PIPE)
    if CASE == "editor-long":
        time.sleep(5.3)
        assert proc.poll() is None, ("ordinary editor must still run after five seconds",
                                    proc.returncode, proc.stderr.read())
        assert not SENTINEL.exists(), "editor phase ended before the long-edit control"
    rc = finish(proc, 5)
    capture = proc.stderr.read()
    assert proc.stdout.read() == b"", "helper stdout must be silent"
    expected = -signal.SIGTERM if CASE == "editor-signal" else 37
    assert rc == expected, ("editor status must survive helper", expected, rc, capture)
    assert capture == b"", ("post-editor path must be silent", capture)
print(f"PASS {CASE}", flush=True)
