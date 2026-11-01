"""Q35-12's coder controls for the editor helper, through the e2e build's seams.

The test process owns the instance and the listening hook socket (inherited
here as fd 3, so the peer's credentials are the instance's daemon). This
script is the scripted daemon peer, the agent that launches the helper (its
identity file names the launching process, so the invocation is `Owned`),
and the observer of the seam trace. Every process and terminal is the
fixture's own; children are killed only through their captured handles.
"""
import atexit
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import subprocess
import sys
import termios
import threading
import time

CASE, HELPER, ROOT = sys.argv[1:]
ROOT = Path(ROOT)
INSTANCE = Path(HELPER).parent
LISTENER = socket.socket(fileno=3)
TRACE = ROOT / "trace"
FIFO = ROOT / "pause"
EXPORT = ROOT / "codex/editor/.tmpfault.md"
EXPORT.parent.mkdir(parents=True)
EXPORT.write_text("")
SENTINEL = ROOT / "editor-finished"
EDITOR = ROOT / "sentinel-editor"
D_NS = 1_000_000_000
SLACK_NS = 400_000_000
NOTICE = (b"dot-agent-deck: the deck was checking this input box. "
          b"If you pressed Ctrl+G, press it again in a moment.\n")
CHILDREN = []

EDITOR.write_text("#!/usr/bin/python3\n" + "\n".join([
    "import json, os, pathlib, subprocess, sys, time",
    "case = os.environ['CHECK_CASE']",
    "out = pathlib.Path(os.environ['CHECK_OUT'])",
    "if case == 'wait-unknown': time.sleep(1.5)",
    "if case == 'sigint-during-edit': time.sleep(1.5)",
    "if case == 'survivors':",
    "    subprocess.Popen(['sleep', '3'], start_new_session=True, stdin=subprocess.DEVNULL,",
    "                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)",
    "if case == 'reaped-descendant':",
    "    # An unwaited child that exits before the editor does: a zombie the",
    "    # helper inherits and reaps.",
    "    subprocess.Popen(['true'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,",
    "                     stderr=subprocess.DEVNULL)",
    "    time.sleep(0.3)",
    "if case == 'inherited-fds':",
    "    (out / 'fds.json').write_text(json.dumps(sorted(int(f) for f in os.listdir('/proc/self/fd'))))",
    "if case == 'traced-editor':",
    "    import ctypes, signal",
    "    libc = ctypes.CDLL(None, use_errno=True)",
    "    # PTRACE_TRACEME: the helper, this editor's parent, becomes its tracer,",
    "    # so the stop at the next signal reaches the helper's plain waitpid.",
    "    if libc.ptrace(0, 0, None, None) != 0:",
    "        (out / 'traceme-failed').write_text(str(ctypes.get_errno()))",
    "        sys.exit(90)",
    "    signal.signal(signal.SIGUSR2, signal.SIG_IGN)",
    "    os.kill(os.getpid(), signal.SIGUSR2)",
    "if case == 'inherited-signals':",
    "    status = pathlib.Path('/proc/self/status').read_text()",
    "    fields = dict(l.split(':\\t', 1) for l in status.splitlines() if ':\\t' in l)",
    "    (out / 'signals.json').write_text(json.dumps({k: fields[k].strip() for k in ('SigBlk', 'SigIgn')}))",
    "(out / 'argv.json').write_text(json.dumps(sys.argv[1:]))",
    "(out / 'editor-finished').write_text('finished')",
    "sys.exit(37)", ""]))
EDITOR.chmod(0o700)


@atexit.register
def cleanup():
    for child in CHILDREN:
        if child.poll() is None:
            child.kill()
            child.wait()


def mono_ns():
    return time.monotonic_ns()


def trace():
    if not TRACE.exists():
        return []
    rows = []
    for line in TRACE.read_text().splitlines():
        ns, _, event = line.partition(" ")
        rows.append((int(ns), event))
    return rows


def event_ns(name):
    for ns, event in trace():
        if event == name or event.startswith(name + " "):
            return ns, event
    raise AssertionError((CASE, "no trace event", name, trace()))


def wait_event(name, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for ns, event in trace():
            if event == name or event.startswith(name + " "):
                return ns
        time.sleep(0.01)
    raise AssertionError((CASE, "trace event never came", name, trace()))


def t0():
    ns, event = event_ns("t0")
    return int(event.split()[1])


def assert_nothing_after(limit_ns, what=("poll", "send", "recv")):
    late = [(ns, e) for ns, e in trace() if ns > limit_ns and e.split()[0] in what]
    assert not late, (CASE, "syscalls after the deadline", late, trace())


def assert_exit_by_deadline():
    start = t0()
    end, _ = event_ns("exit")
    assert end - start <= D_NS + SLACK_NS, (CASE, "exit after D", end - start, trace())
    assert_nothing_after(start + D_NS)
    return end - start


def write_identity():
    """`gen-3` naming this process as the agent `A`, so its helper is Owned."""
    stat = Path("/proc/self/stat").read_text()
    start = stat[stat.rfind(")") + 2:].split()[19]
    exe = os.stat("/proc/self/exe")
    real = os.path.realpath("/proc/self/exe")
    root = os.path.dirname(real)
    root_st = os.stat(root)
    text = (f"pid={os.getpid()}\nstart={start}\nexe={exe.st_dev}:{exe.st_ino}\n"
            f"root_id={root_st.st_dev}:{root_st.st_ino}\nroot={root.encode().hex()}\n"
            f"basename={os.path.basename(real).encode().hex()}\n")
    fd = os.open(INSTANCE / "gen-3", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
    os.write(fd, text.encode())
    os.close(fd)


class Peer(threading.Thread):
    """The daemon's side of one connection: the capability question, the
    invocation, then what `post` does with the report."""

    def __init__(self, post, frontier=False, outcome=b'{"outcome":{"kind":"unarmed"}}'):
        super().__init__(daemon=True)
        self.post, self.frontier, self.outcome = post, frontier, outcome
        self.lines, self.report, self.after = [], b"", b""
        self.done = threading.Event()

    def readline(self, conn):
        line = b""
        while not line.endswith(b"\n"):
            chunk = conn.recv(1)
            if not chunk:
                raise EOFError(line)
            line += chunk
        return json.loads(line)

    def run(self):
        try:
            LISTENER.settimeout(15)
            conn, _ = LISTENER.accept()
            conn.settimeout(15)
            self.lines.append(self.readline(conn))
            caps = {"composer_probe": 1}
            if self.frontier:
                caps["editor_frontier"] = 1
            conn.sendall(json.dumps(caps).encode() + b"\n")
            self.lines.append(self.readline(conn))
            conn.sendall(self.outcome + b"\n")
            self.post(self, conn)
        finally:
            self.done.set()


def read_report(peer, conn):
    data = b""
    while not data.endswith(b"\n"):
        chunk = conn.recv(65536)
        if not chunk:
            break
        data += chunk
    peer.report = data
    return data


def post_read_to_eof(peer, conn):
    data = b""
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            break
        data += chunk
    peer.report = data


def post_ack(peer, conn):
    read_report(peer, conn)
    conn.sendall(b'{"frontier":"taken"}\n')
    while chunk := conn.recv(65536):
        peer.after += chunk


def post_no_ack(peer, conn):
    read_report(peer, conn)
    conn.settimeout(5)
    try:
        peer.after = conn.recv(1)
    except socket.timeout:
        peer.after = b"timeout"


def post_half_ack(peer, conn):
    read_report(peer, conn)
    conn.sendall(b'{"frontier":')
    conn.settimeout(5)
    try:
        peer.after = conn.recv(1)
    except socket.timeout:
        peer.after = b"timeout"


def post_hang_up(peer, conn):
    conn.close()


def post_unread(peer, conn):
    time.sleep(4)


def post_half_frame(peer, conn):
    want = int(os.environ["HALF"])
    got = b""
    while len(got) < want:
        chunk = conn.recv(min(65536, want - len(got)))
        if not chunk:
            break
        got += chunk
    peer.report = got
    time.sleep(4)


def env(extra=None):
    base = dict(os.environ)
    base.update(DOT_AGENT_DECK_EDITOR_GEN="3",
                DOT_AGENT_DECK_SOCKET=str(LISTENER.getsockname()),
                DOT_AGENT_DECK_PANE_ID="fixture",
                DOT_AGENT_DECK_PANE_CAPABILITY="fixture",
                CODEX_HOME=str(ROOT / "codex"),
                VISUAL=HELPER, EDITOR=HELPER,
                DOT_AGENT_DECK_EDITOR_USER_VISUAL=str(EDITOR),
                DOT_AGENT_DECK_EDITOR_USER_EDITOR=str(EDITOR),
                DOT_AGENT_DECK_E2E_HELPER_TRACE=str(TRACE),
                CHECK_CASE=CASE, CHECK_OUT=str(ROOT))
    base.update(extra or {})
    return base


def launch(fds, extra=None, preexec=None, pass_fds=()):
    child = subprocess.Popen([HELPER, str(EXPORT)], env=env(extra), cwd=ROOT,
                             stdin=fds if fds >= 0 else subprocess.DEVNULL,
                             stdout=fds, stderr=fds, preexec_fn=preexec, pass_fds=pass_fds)
    CHILDREN.append(child)
    return child


def finish(proc, timeout=8):
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        raise AssertionError((CASE, "helper did not exit", trace()))


def release(proc, timeout=10):
    """Write one byte to the pause FIFO once the helper has it open for
    reading, rather than blocking in open(2) for a reader that never comes:
    a helper that exited, or a pause that already passed, fails here with
    the trace instead of hanging the test."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            fd = os.open(FIFO, os.O_WRONLY | os.O_NONBLOCK)
        except OSError as error:
            if error.errno != errno.ENXIO:
                raise
            assert proc.poll() is None, (CASE, "no pause left to release", trace())
            assert time.monotonic() < deadline, (CASE, "no reader at the pause", trace())
            time.sleep(0.005)
            continue
        try:
            os.write(fd, b"x")
        finally:
            os.close(fd)
        return


def holds_fifo(pid):
    """Whether process `pid` has the pause FIFO open."""
    for fd in os.listdir(f"/proc/{pid}/fd"):
        try:
            if os.readlink(f"/proc/{pid}/fd/{fd}") == str(FIFO):
                return True
        except OSError:
            pass
    return False


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


def terminal(mode):
    master, slave = pty.openpty()
    attrs = termios.tcgetattr(slave)
    attrs[3] &= ~(termios.ECHO | termios.ICANON)
    termios.tcsetattr(slave, termios.TCSANOW, attrs)
    prefix = b""
    if mode == "stopped":
        termios.tcflow(slave, termios.TCOOFF)
    if mode == "full":
        flags = fcntl.fcntl(slave, fcntl.F_GETFL)
        fcntl.fcntl(slave, fcntl.F_SETFL, flags | os.O_NONBLOCK)
        stable = 0
        while stable < 3:
            try:
                prefix += b"P" * os.write(slave, b"P" * 4096)
                stable = 0
            except BlockingIOError:
                stable += 1
                time.sleep(0.02)
        fcntl.fcntl(slave, fcntl.F_SETFL, flags)
        assert prefix
    return master, slave, prefix


def in_session(body):
    """Run `body` in a forked supervisor that owns a fixture session, so the
    helper's controlling terminal is the fixture's and nothing hangs it up."""
    supervisor = os.fork()
    if supervisor == 0:
        try:
            body()
            os._exit(0)
        except BaseException:
            import traceback
            traceback.print_exc()
            os._exit(1)
    _, status = os.waitpid(supervisor, 0)
    assert os.waitstatus_to_exitcode(status) == 0, (CASE, "terminal scenario failed")


def terminal_row(mode, extra, expect_rc, post, check):
    """One helper on a fixture terminal in `mode` (`stopped`, `full` or
    `background`), with a peer, asserting zero helper bytes at the master."""
    def body():
        master, slave, prefix = terminal("normal" if mode == "background" else mode)
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(slave, os.getpgrp())
        preexec = None
        if mode == "background":
            attrs = termios.tcgetattr(slave)
            attrs[3] |= termios.TOSTOP
            termios.tcsetattr(slave, termios.TCSANOW, attrs)

            def preexec():
                os.setpgid(0, 0)
                signal.signal(signal.SIGTTOU, signal.SIG_DFL)
                signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTTOU})
        write_identity()
        peer = Peer(post)
        peer.start()
        proc = launch(slave, extra, preexec)
        deadline = time.monotonic() + 8
        while True:
            pid, status = os.waitpid(proc.pid, os.WNOHANG | os.WUNTRACED)
            if pid:
                assert not os.WIFSTOPPED(status), (CASE, "helper stopped", os.WSTOPSIG(status))
                proc.returncode = os.waitstatus_to_exitcode(status)
                break
            assert time.monotonic() < deadline, (CASE, "helper did not exit", trace())
            time.sleep(0.01)
        if mode == "stopped":
            termios.tcflow(slave, termios.TCOON)
        capture = read_master(master)
        assert proc.returncode == expect_rc, (CASE, mode, proc.returncode, trace())
        assert capture == prefix, (CASE, mode, "helper terminal bytes", capture[len(prefix):][:200])
        assert_exit_by_deadline()
        peer.done.wait(10)
        check(peer)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        os.close(slave)
        os.close(master)
    in_session(body)


def spawn_failed_check(peer):
    assert json.loads(peer.report) == {"report": "spawn_failed", "errno": 13}, peer.report
    assert peer.lines[1]["message_type"] == "composer_invocation", peer.lines
    assert peer.lines[1]["decision"] == "unarmed", peer.lines
    assert not SENTINEL.exists(), "no editor ran"


def wait_unknown_check(peer):
    assert peer.report == b"", ("WaitUnknown sends no report", peer.report)
    deadline = time.monotonic() + 5
    while not SENTINEL.exists():
        assert time.monotonic() < deadline, "the editor survived the helper and finished"
        time.sleep(0.05)


def set_arm(count, length=None):
    """Rewrite the arm page's three words as one even publication, with no
    reader running, and optionally cut the file to `length` bytes."""
    with open(INSTANCE / "arm-3", "r+b") as page:
        page.write(struct.pack("<QQQ", (count << 32) | 4, 17, 23))
        if length is not None:
            page.truncate(length)


def pipe_row(extra, post, frontier=False, during=None, outcome=None, pass_fds=()):
    write_identity()
    peer = Peer(post, frontier, **({"outcome": outcome} if outcome else {}))
    peer.start()
    proc = launch(subprocess.PIPE, extra, pass_fds=pass_fds)
    if during:
        during(proc)
    rc = finish(proc)
    out, err = proc.stdout.read(), proc.stderr.read()
    assert out == b"" and err == b"", (CASE, "helper output", out, err)
    peer.done.wait(10)
    return rc, peer


SPAWN_ERRNO = {"DOT_AGENT_DECK_E2E_HELPER_SPAWN_ERRNO": "13"}
WAIT_ERRNO = {"DOT_AGENT_DECK_E2E_HELPER_WAIT_ERRNO": "22"}

if CASE == "spawn-failed-pipe":
    rc, peer = pipe_row(SPAWN_ERRNO, post_read_to_eof)
    assert rc == 127, rc
    spawn_failed_check(peer)
    assert any(e == "spawn-failed Some(13)" for _, e in trace()), trace()
    assert_exit_by_deadline()
elif CASE.startswith("spawn-failed-"):
    terminal_row(CASE.split("-")[2], SPAWN_ERRNO, 127, post_read_to_eof, spawn_failed_check)
elif CASE == "wait-unknown-pipe":
    rc, peer = pipe_row(WAIT_ERRNO, post_read_to_eof)
    assert rc == 1, rc
    assert any(e == "wait-unknown 22" for _, e in trace()), trace()
    assert_exit_by_deadline()
    wait_unknown_check(peer)
elif CASE.startswith("wait-unknown-"):
    terminal_row(CASE.split("-")[2], WAIT_ERRNO, 1, post_read_to_eof, wait_unknown_check)
elif CASE == "expired-deadline":
    os.mkfifo(FIFO)
    extra = {"DOT_AGENT_DECK_E2E_HELPER_PAUSE": str(FIFO),
             "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT": "post-editor"}

    def hold_past_deadline(proc):
        wait_event("pause post-editor")
        start = t0()
        while mono_ns() < start + D_NS + 300_000_000:
            time.sleep(0.02)
        released = mono_ns()
        with open(FIFO, "wb") as fifo:
            fifo.write(b"x")
        proc.released = released

    rc, peer = pipe_row(extra, post_read_to_eof, during=hold_past_deadline)
    assert rc == 37, rc
    assert peer.report == b"", ("no report after D", peer.report)
    resumed = wait_event("resume post-editor")
    assert any(e == "skip report" for _, e in trace()), trace()
    assert any(e == "tree Unknown" for _, e in trace()), ("the reap loop was skipped too", trace())
    assert_nothing_after(t0() + D_NS)
    end, _ = event_ns("exit")
    assert end - resumed < 200_000_000, ("exit follows release at once", end - resumed)
elif CASE == "ready-past-deadline":
    # U2-01: the pause point sits between a poll that found the socket ready
    # and the call it allows. Each pause before the editor ends is released
    # at once; the first after `t0` (the report's POLLOUT) is held until
    # 300 ms past D, so that D passes between the ready poll and the send.
    os.mkfifo(FIFO)
    extra = {"DOT_AGENT_DECK_E2E_HELPER_PAUSE": str(FIFO),
             "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT": "ready"}

    def hold_ready_past_deadline(proc):
        released = 0
        deadline = time.monotonic() + 10
        while True:
            rows = trace()
            pauses = sum(1 for _, e in rows if e == "pause ready")
            if pauses > released:
                if any(e.startswith("t0 ") for _, e in rows):
                    break
                release(proc)
                released += 1
            assert proc.poll() is None and time.monotonic() < deadline, (CASE, rows)
            time.sleep(0.005)
        assert released >= 2, ("the exchange's ready polls paused too", released, trace())
        start = t0()
        while mono_ns() < start + D_NS + 300_000_000:
            time.sleep(0.02)
        release(proc)

    rc, peer = pipe_row(extra, post_read_to_eof, during=hold_ready_past_deadline)
    assert rc == 37, rc
    assert peer.report == b"", ("no report once D passed after a ready poll", peer.report)
    rows = trace()
    held = [ns for ns, e in rows if e == "resume ready"][-1]
    assert held > t0() + D_NS, ("resumed past D", held - t0())
    after = [e for ns, e in rows if ns >= held]
    assert after[:2] == ["resume ready", "skip io"], ("the deadline rechecked first", after)
    late = [e for e in after if e.split()[0] in ("poll", "send", "recv")]
    assert not late, ("no call after D", late, rows)
    assert "report unsent" in after, rows
    end, _ = event_ns("exit")
    assert end - held < 200_000_000, ("exit follows release at once", end - held)
elif CASE == "stale-pause-writer":
    # The CI hang of `ready-past-deadline` (run 41255067763): the writer that
    # released one pause was still open when the helper opened the FIFO for
    # its next, so that open succeeded at once and the writer's close read as
    # end of file. The seam took that as a release, with no byte written.
    # Here the first pause's writer is held open until the helper holds the
    # FIFO for its second pause, then closed: the second pause must wait for
    # a byte of its own.
    os.mkfifo(FIFO)
    extra = {"DOT_AGENT_DECK_E2E_HELPER_PAUSE": str(FIFO),
             "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT": "ready"}

    BYTE_AT = []

    def count(name):
        return sum(1 for _, e in trace() if e == name)

    def stale_then_release(proc):
        wait_event("pause ready")
        fd = os.open(FIFO, os.O_WRONLY)
        os.write(fd, b"x")
        deadline = time.monotonic() + 10
        while not (count("pause ready") >= 2 and holds_fifo(proc.pid)):
            assert proc.poll() is None and time.monotonic() < deadline, (CASE, trace())
            time.sleep(0.005)
        os.close(fd)
        wait_event("pause-eof ready", timeout=5)
        time.sleep(0.3)
        assert count("resume ready") == 1, ("the second pause passed on end of file", trace())
        BYTE_AT.append(mono_ns())
        release(proc)
        # The rest of the exchange and the report: each pause released once,
        # counted, since a helper blocked opening the FIFO holds no
        # descriptor yet.
        released = 2
        while proc.poll() is None:
            if count("pause ready") > released:
                release(proc)
                released += 1
            time.sleep(0.005)

    rc, peer = pipe_row(extra, post_read_to_eof, during=stale_then_release)
    assert rc == 37, rc
    rows = trace()
    assert sum(1 for _, e in rows if e == "pause-eof ready") == 1, rows
    pauses = sum(1 for _, e in rows if e == "pause ready")
    assert pauses >= 3 and pauses == sum(1 for _, e in rows if e == "resume ready"), rows
    second = [ns for ns, e in rows if e == "resume ready"][1]
    assert second > BYTE_AT[0], ("the second pause resumed on its own byte", second - BYTE_AT[0], rows)
    assert peer.report.endswith(b"\n"), ("the report still went out", peer.report)
elif CASE == "continued-command":
    # U2-02: Codex's shlex joins a backslash-newline and drops a comment, so
    # this saved VISUAL opens the editor with two arguments; a quote inside
    # the comment is commented out too.
    value = f"{EDITOR}\\\n --continued # it's a note\n--after"
    rc, peer = pipe_row({"DOT_AGENT_DECK_EDITOR_USER_VISUAL": value}, post_read_to_eof)
    assert rc == 37 and SENTINEL.exists(), ("the continued command opened", rc)
    argv = json.loads((ROOT / "argv.json").read_text())
    assert argv == ["--continued", "--after", str(EXPORT)], argv
    assert json.loads(peer.report) == {"report": "editor_done", "tree": "clean"}, peer.report
elif CASE == "inherited-fds":
    # Core §5.3: the agent leaves descriptor 57 inheritable; the helper gets
    # it, the editor doesn't.
    extra_fd = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY)
    os.dup2(extra_fd, 57)
    os.close(extra_fd)
    rc, peer = pipe_row({}, post_read_to_eof, pass_fds=(57,))
    assert rc == 37, rc
    fds = json.loads((ROOT / "fds.json").read_text())
    assert 57 not in fds, ("an inherited descriptor reached the editor", fds)
elif CASE == "traced-editor":
    # The editor makes the helper its tracer and takes a signal: the stop
    # reaches the helper's waitpid, which mustn't read it as an exit. The
    # helper takes it as an unknown wait: no report, exit 1, and the editor,
    # released when the helper exits, finishes on its own.
    rc, peer = pipe_row({}, post_read_to_eof)
    failed = ROOT / "traceme-failed"
    assert not failed.exists(), ("PTRACE_TRACEME refused here, errno", failed.read_text())
    assert rc == 1, ("a stop is not an exit", rc, trace())
    assert any(e.startswith("wait-nonterminal ") for _, e in trace()), trace()
    assert peer.report == b"", ("no report for a stopped editor", peer.report)
    assert_exit_by_deadline()
    wait_unknown_check(peer)
elif CASE in ("unread-peer", "half-frame"):
    pad = 4 << 20
    os.environ["HALF"] = str(pad // 2)
    extra = {"DOT_AGENT_DECK_E2E_HELPER_SNDBUF": "4096",
             "DOT_AGENT_DECK_E2E_HELPER_PAD": str(pad)}
    rc, peer = pipe_row(extra, post_unread if CASE == "unread-peer" else post_half_frame)
    assert rc == 37, rc
    sent = [int(e.split()[1]) for _, e in trace() if e.startswith("sent ")]
    assert any(0 < n < pad for n in sent), ("a partial send", trace())
    assert any(e == "report unsent" for _, e in trace()), trace()
    assert_exit_by_deadline()
    if CASE == "half-frame":
        assert len(peer.report) == pad // 2, len(peer.report)
elif CASE in ("absent-ack", "eintr-partial-ack"):
    extra = {}
    during = None
    if CASE == "eintr-partial-ack":
        extra = {"DOT_AGENT_DECK_E2E_HELPER_EINTR": "1"}

        def during(proc):
            wait_event("report sent")
            stop = time.monotonic() + 1.2
            while time.monotonic() < stop and proc.poll() is None:
                os.kill(proc.pid, signal.SIGUSR1)
                time.sleep(0.02)

    post = post_no_ack if CASE == "absent-ack" else post_half_ack
    rc, peer = pipe_row(extra, post, frontier=True, during=during)
    assert rc == 37, rc
    assert json.loads(peer.report) == {"report": "editor_done", "tree": "clean"}, peer.report
    assert any(e == "ack false" for _, e in trace()), trace()
    waited = assert_exit_by_deadline()
    assert waited >= 900_000_000, ("the ack was waited for until D", waited)
    assert any(e == "spawn tasks 1" for _, e in trace()), ("single-threaded at spawn", trace())
    if CASE == "eintr-partial-ack":
        assert any(e == "polled -1" for _, e in trace()), ("an interrupted poll", trace())
        assert any(e.startswith("received ") and int(e.split()[1]) > 0 for _, e in trace()), trace()
elif CASE == "clean-ack":
    rc, peer = pipe_row({}, post_ack, frontier=True)
    assert rc == 37, rc
    assert json.loads(peer.report) == {"report": "editor_done", "tree": "clean"}, peer.report
    assert any(e == "ack true" for _, e in trace()), trace()
    assert json.loads((ROOT / "argv.json").read_text()) == [str(EXPORT)]
    assert peer.lines[0]["message_type"] == "hook_capabilities", peer.lines
    request = peer.lines[1]
    assert (request["decision"], request["claim"], request["generation"]) == ("unarmed", "owned", 3)
    assert (request["seg_seen"], request["native_seen"], request["argv"]) == (17, 23, [str(EXPORT)])
    markers = os.listdir(INSTANCE / "inv/3")
    assert len(markers) == 1 and markers[0].endswith("-unarmed-17-23"), markers
elif CASE == "reaped-descendant":
    rc, peer = pipe_row({}, post_ack, frontier=True)
    assert rc == 37, rc
    assert json.loads(peer.report) == {"report": "editor_done", "tree": "clean"}, peer.report
elif CASE == "survivors":
    rc, peer = pipe_row({}, post_read_to_eof, frontier=True)
    assert rc == 37, rc
    assert json.loads(peer.report) == {"report": "editor_done", "tree": "survivors"}, peer.report
    assert not any(e.startswith("ack ") for _, e in trace()), "no ack wait for survivors"
elif CASE == "inherited-signals":
    def ignore():
        for sig in (signal.SIGALRM, signal.SIGINT, signal.SIGCHLD):
            signal.signal(sig, signal.SIG_IGN)
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM, signal.SIGUSR2})
    write_identity()
    peer = Peer(post_read_to_eof)
    peer.start()
    proc = subprocess.Popen([HELPER, str(EXPORT)], env=env(), cwd=ROOT,
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, preexec_fn=ignore)
    CHILDREN.append(proc)
    assert finish(proc) == 37, "the helper still reaps with SIGCHLD inherited ignored"
    seen = json.loads((ROOT / "signals.json").read_text())
    mask = lambda sigs: sum(1 << (s - 1) for s in sigs)
    ignored = int(seen["SigIgn"], 16)
    blocked = int(seen["SigBlk"], 16)
    want_ign = mask([signal.SIGALRM, signal.SIGINT, signal.SIGCHLD])
    assert ignored & want_ign == want_ign, ("dispositions restored", seen)
    # Python ignores SIGPIPE itself at startup, so only SIGQUIT is checked.
    assert ignored & mask([signal.SIGQUIT]) == 0, ("SIGQUIT back to its default", seen)
    assert blocked == mask([signal.SIGALRM, signal.SIGUSR2]), ("mask restored", seen)
elif CASE == "sigint-during-edit":
    def interrupt(proc):
        while not any(e.startswith("spawn ") for _, e in trace()):
            time.sleep(0.02)
        time.sleep(0.3)
        os.kill(proc.pid, signal.SIGINT)
        os.kill(proc.pid, signal.SIGQUIT)
    rc, peer = pipe_row({}, post_read_to_eof, during=interrupt)
    assert rc == 37, ("the helper ignored SIGINT and SIGQUIT and returned the editor's status", rc)
elif CASE == "literal-tool-value":
    # No identity: the invocation is Unknown, and this argument has no export
    # shape, so the tool path runs the saved value, split, never evaluated.
    # The editor is named by its basename, found on the editor's PATH after
    # a directory and a non-executable file of the same name, which execvp
    # passes over.
    (ROOT / "decoy-dir" / EDITOR.name).mkdir(parents=True)
    (ROOT / "decoy-file").mkdir()
    (ROOT / "decoy-file" / EDITOR.name).write_text("#!/bin/sh\nexit 99\n")
    (ROOT / "decoy-file" / EDITOR.name).chmod(0o600)
    hostile = f"{EDITOR.name} ';' '$(touch {ROOT}/pwned)' '|' '>{ROOT}/out' `id`"
    path = f"{ROOT}/decoy-dir:{ROOT}/decoy-file:{ROOT}:/usr/bin:/bin"
    proc = subprocess.Popen([HELPER, str(ROOT / "COMMIT_EDITMSG")],
                            env=env({"DOT_AGENT_DECK_EDITOR_USER_VISUAL": hostile,
                                     "PATH": path, "HOME": str(ROOT / "decoy-dir")}),
                            # Where `./<name>`, like `$HOME/<name>`, is the decoy
                            # directory, so only the PATH lookup finds the editor.
                            cwd=ROOT / "decoy-dir", stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    CHILDREN.append(proc)
    assert finish(proc) == 37
    argv = json.loads((ROOT / "argv.json").read_text())
    assert argv == [";", f"$(touch {ROOT}/pwned)", "|", f">{ROOT}/out", "`id`",
                    str(ROOT / "COMMIT_EDITMSG")], argv
    assert not (ROOT / "pwned").exists() and not (ROOT / "out").exists()
elif CASE in ("mutant-a5", "design-a5-row"):
    # Amendment 6 §2.4: A5's no-op alarm before a blocking write. The pause
    # holds the mutant after `setitimer` until its handler has run, then
    # lets the write meet a stopped terminal. No peer: Unknown, unreported.
    os.mkfifo(FIFO)
    extra = dict(SPAWN_ERRNO)
    if CASE == "mutant-a5":
        extra.update({"DOT_AGENT_DECK_E2E_HELPER_MUTANT": "a5-alarm",
                      "DOT_AGENT_DECK_E2E_HELPER_PAUSE": str(FIFO),
                      "DOT_AGENT_DECK_E2E_HELPER_PAUSE_AT": "mutant"})

    def body():
        master, slave, prefix = terminal("stopped")
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(slave, os.getpgrp())
        proc = launch(slave, extra)
        if CASE == "mutant-a5":
            wait_event("alarm-handler-ran")
            with open(FIFO, "wb") as fifo:
                fifo.write(b"x")
            start = t0()
            while mono_ns() < start + D_NS + 2_000_000_000:
                time.sleep(0.05)
            assert proc.poll() is None, ("RED: the mutant is still blocked 2 s past D", trace())
            proc.kill()
            proc.wait()
        else:
            assert finish(proc) == 127
            assert_exit_by_deadline()
        termios.tcflow(slave, termios.TCOON)
        capture = read_master(master)
        if CASE == "design-a5-row":
            assert capture == prefix, ("no helper bytes", capture)
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        os.close(slave)
        os.close(master)
    in_session(body)
elif CASE in ("armed-uncertain", "armed-mine-empty"):
    set_arm(1)
    uncertain = CASE == "armed-uncertain"
    outcome = (b'{"outcome":{"kind":"uncertain"}}' if uncertain
               else b'{"outcome":{"kind":"mine","read":"empty"}}')
    write_identity()
    peer = Peer(post_read_to_eof, outcome=outcome)
    peer.start()
    started = time.monotonic()
    proc = launch(subprocess.PIPE)
    rc = finish(proc)
    elapsed = time.monotonic() - started
    err = proc.stderr.read()
    peer.done.wait(10)
    request = peer.lines[1]
    assert (request["decision"], request["claim"]) == ("armed", "owned"), request
    assert (request["seg_seen"], request["native_seen"]) == (17, 23), request
    markers = os.listdir(INSTANCE / "inv/3")
    assert len(markers) == 1 and markers[0].endswith("-armed-17-23"), markers
    assert not SENTINEL.exists(), "the armed path opens no editor"
    assert peer.report == b"", ("the armed path sends no report", peer.report)
    if uncertain:
        assert (rc, err) == (1, NOTICE), (rc, err)
        assert elapsed >= 1.45, ("the notice stays 1.5 s", elapsed)
    else:
        assert (rc, err) == (0, b""), (rc, err)
        assert elapsed < 1.2, ("no notice wait", elapsed)
elif CASE == "peer-gone":
    rc, peer = pipe_row({}, post_hang_up)
    assert rc == 37, ("a failed report leaves the editor's status", rc)
    assert any(e == "report unsent" for _, e in trace()), trace()
    waited = assert_exit_by_deadline()
    assert waited < 500_000_000, ("a failed send isn't retried to the deadline", waited, trace())
elif CASE == "peer-eof":
    # The peer reads the capability question and hangs up without an
    # answer: the exchange fails at once, not at its 2 s bound.
    write_identity()

    def closing_peer():
        LISTENER.settimeout(15)
        conn, _ = LISTENER.accept()
        conn.settimeout(5)
        while not conn.recv(65536).endswith(b"\n"):
            pass
        conn.close()

    thread = threading.Thread(target=closing_peer, daemon=True)
    thread.start()
    started = time.monotonic()
    proc = launch(subprocess.PIPE)
    rc = finish(proc)
    elapsed = time.monotonic() - started
    thread.join(10)
    assert rc == 37 and SENTINEL.exists(), rc
    assert elapsed < 1.5, ("a closed connection ends the exchange at once", elapsed)
elif CASE == "exec-fails":
    # A real failed start, without the seam: an executable file that isn't
    # a program, so execve fails with ENOEXEC in the child.
    bad = ROOT / "not-a-program"
    bad.write_bytes(b"\x00\x01 not an executable format\n")
    bad.chmod(0o700)
    rc, peer = pipe_row({"DOT_AGENT_DECK_EDITOR_USER_VISUAL": str(bad)}, post_read_to_eof)
    assert rc == 127, rc
    assert json.loads(peer.report) == {"report": "spawn_failed", "errno": errno.ENOEXEC}, peer.report
    assert_exit_by_deadline()
elif CASE == "arm-exact-words":
    # A page exactly as long as its three words, count 0, reads absent.
    set_arm(0, 24)
    proc = launch(subprocess.PIPE)
    rc = finish(proc)
    err = proc.stderr.read()
    assert (rc, err) == (37, b""), ("absent: the user's editor, no notice", rc, err)
    assert SENTINEL.exists()
elif CASE == "codex-empty-visual":
    rc, peer = pipe_row({"DOT_AGENT_DECK_EDITOR_USER_VISUAL": ""}, post_read_to_eof)
    assert rc == 1, ("Codex refuses an empty VISUAL", rc)
    assert json.loads(peer.report) == {"report": "spawn_failed"}, peer.report
    assert not SENTINEL.exists(), "no editor ran"
elif CASE in ("foreign-start", "foreign-boot"):
    # The record names this socket's listener with another start time, or
    # another boot: the helper sends it nothing, and runs the editor.
    write_identity()
    got = []

    def silent_peer():
        LISTENER.settimeout(15)
        conn, _ = LISTENER.accept()
        conn.settimeout(6)
        try:
            while chunk := conn.recv(65536):
                got.append(chunk)
        except socket.timeout:
            got.append(b"<timeout>")

    thread = threading.Thread(target=silent_peer, daemon=True)
    thread.start()
    proc = launch(subprocess.PIPE)
    rc = finish(proc)
    thread.join(10)
    assert rc == 37, rc
    assert SENTINEL.exists()
    assert got == [], ("nothing sent to a peer the record doesn't name", got)
else:
    raise AssertionError(f"unknown case {CASE}")
print(f"PASS {CASE}", flush=True)
