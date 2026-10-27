#!/usr/bin/env python3
"""H-FRX: opt-in local measurement tooling; default actions start no agents.

Only run-x2 executes Codex. X1/X3/X4/M-FIRST await reviewed part-3 seams.
See docs/develop/hfrx.md for the boundary and the evidence contract.
"""

from __future__ import annotations

import argparse
import ast
import ctypes
import errno
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import re
import selectors
import shlex
import signal
import stat
import struct
import subprocess
import sys
import termios
import threading
import time

FAKE_KEY = "sk-fixture-not-a-key"
FIXTURE = "# H-FRX fixture; no inherited deck configuration\n"
ENABLE = b"\x1b[?2004h"
DISABLE = b"\x1b[?2004l"
FRAME = b"\x1b[?2026h"
VERSIONS = ("0.160.0", "0.164.0")
ROWS = {
    "X1": {"status": "needs-implementation", "runs_per_setup": 50,
           "needs": ["Q35-12 helper", "Q35-20a frontier and dual-clock log",
                     "silent foreground stand-in driver (10 s after frontier)"]},
    "X2-arrival": {"status": "runnable-now", "runs_per_release": 100,
                   "label": "master-observed arrival interval; no elapsed-time bound"},
    "X2-instrumented": {"status": "runnable-now", "runs_per_release": 100,
                        "label": "instrumented write-to-flush interval; tracing changes timing",
                        "frontier_column": "needs Q35-20a; unavailable until then"},
    "X3-gate": {"status": "needs-implementation", "calibration_only": True,
                "needs": ["disposable daemon e2e-only Authority override", "Q35-20 EC log",
                          "reviewed tcflush_gate supervisor", "offset-triggered writer"]},
    "X3-natural": {"status": "needs-implementation", "calibration_only": True,
                   "delays_us": [0, 50, 100, 200, 500, 1000, 2000, 5000],
                   "needs": ["disposable daemon e2e-only Authority override", "EC log",
                             "offset-triggered writer"]},
    "X4": {"status": "needs-implementation", "runs_per_setup": 50,
           "editor_eaten_runs_per_setup": 20, "policy": "Hold",
           "label": "EC would have completed before the flush",
           "needs": ["Q35-20a frontier", "Q35-20 evaluation observer", "real editor drivers"]},
    "M-FIRST": {"status": "needs-implementation", "platforms": ["Linux", "macOS"],
                "needs": ["Q35-07 one structured observation per generation; policy Hold"]},
}
EDITORS = ["vim", "nvim", "nvim+lsp", "nano", "emacs-nw", "emacsclient-t",
           "code-closed", "code-open", "hx", "distrobox-nvim", "toolbox-nvim",
           "nsenter-rootless", "user-named-copied-setup"]
FIRST_FIELDS = ["platform", "generation", "duration_ns", "first_stop_ns", "cause",
                "attribution", "held_input_discarded_at_restart", "pending_count",
                "pending_median_ns", "pending_max_ns", "pending_median_method",
                "pending_resolutions"]
CAUSES = {"NativeEditor", "CheckEditor", "ToolEditor", "ShapeEditor", "UnknownEditor",
          "UnarmedThenSpawnFailed", "Survivors", "UnknownReport", "DroppedObservation",
          "EditorUnconfirmed", "ScanDroppedAtCap"}


class Refusal(RuntimeError):
    pass


class StopRequested(Exception):
    pass


def write_json(path, value):
    with Path(path).open("x", encoding="utf-8") as out:
        json.dump(value, out, indent=2, sort_keys=True)
        out.write("\n")


def under(path, root):
    return path == root or root in path.parents


def production_sockets(env):
    """Path computations only; do not probe, open or connect to production."""
    runtime = env.get("XDG_RUNTIME_DIR")
    fallback = Path("/tmp") / f"dot-agent-deck-{os.getuid()}"
    paths = {fallback / "hook.sock", fallback / "attach.sock",
             Path(f"/tmp/dot-agent-deck-{os.getuid()}.sock"),
             Path(f"/tmp/dot-agent-deck-attach-{os.getuid()}.sock"),
             Path("/tmp/dot-agent-deck.sock"), Path("/tmp/dot-agent-deck-attach.sock")}
    if runtime:
        paths.update([Path(runtime) / "dot-agent-deck.sock",
                      Path(runtime) / "dot-agent-deck-attach.sock"])
    for name in ("DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"):
        if env.get(name):
            paths.add(Path(os.path.abspath(env[name])))
    return paths


def fixture_env(root):
    return {
        "PATH": str(root / "bin"), "TERM": "xterm-256color", "LANG": "C.UTF-8",
        "HOME": str(root / "home"), "CODEX_HOME": str(root / "codex-home"),
        **{f"XDG_{kind}_HOME": str(root / "xdg" / kind.lower())
           for kind in ("CONFIG", "DATA", "STATE", "CACHE")},
        "XDG_RUNTIME_DIR": str(root / "xdg/runtime"),
        "DOT_AGENT_DECK_STATE_DIR": str(root / "state"),
        "DOT_AGENT_DECK_SOCKET": str(root / "hook.sock"),
        "DOT_AGENT_DECK_ATTACH_SOCKET": str(root / "attach.sock"),
        "DOT_AGENT_DECK_LOG": str(root / "deck.log"),
        "DOT_AGENT_DECK_EXPERIMENTAL": "1", "DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS": "0",
    }


def disk_backed(root):
    # mountinfo is metadata; no agent configuration is opened. Refuse unknown fs.
    candidates = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        left, right = line.split(" - ", 1)
        mount = Path(re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), left.split()[4]))
        if under(root, mount):
            candidates.append((len(mount.parts), right.split()[0]))
    known_disk_filesystems = {"ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs"}
    if not candidates or max(candidates)[1] not in known_disk_filesystems:
        raise Refusal("run root is not on a recognized disk-backed filesystem")


def check_root_location(root):
    home = Path(os.path.abspath(os.environ.get("HOME", "/")))
    if any(under(root, home / name) for name in (".codex", ".claude")):
        raise Refusal("run root must not be inside a real agent configuration directory")
    for ancestor in (root, *root.parents):
        if ancestor.is_symlink():
            raise Refusal("run-root ancestry contains a symlink")


def preflight(root, env, forbidden):
    root = Path(os.path.abspath(root))
    # Reject run-tree symlinks before resolving endpoints, including dangling links.
    check_root_location(root)
    if root.stat().st_uid != os.getuid() or stat.S_IMODE(root.stat().st_mode) != 0o700:
        raise Refusal("run root must be owned by this uid with mode 0700")
    disk_backed(root)
    for path in root.rglob("*"):
        if path.is_symlink() and not under(path, root / "bin"):
            raise Refusal("symlink outside fixture bin")
    own = root / "work/.dot-agent-deck.toml"
    if own.read_text() != FIXTURE:
        raise Refusal("fixture configuration changed")
    for start in (root / "work", root / "home"):
        for ancestor in (start, *start.parents):
            config = ancestor / ".dot-agent-deck.toml"
            if os.path.lexists(config) and config != own:
                raise Refusal(f"foreign .dot-agent-deck.toml in ancestry: {config}")
    if any(path.name == "auth.json" for path in (root / "codex-home").rglob("*")):
        raise Refusal("auth.json present in fixture CODEX_HOME")
    if env != fixture_env(root):
        # Distinguish the most dangerous error without reading a production socket.
        for name in ("DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"):
            candidate = Path(os.path.abspath(env.get(name, "/")))
            if candidate in forbidden or not under(candidate, root):
                raise Refusal("production or non-run deck socket path")
        raise Refusal("environment differs from the closed H-FRX allowlist")
    for name, value in env.items():
        if name.endswith(("_HOME", "_DIR", "_SOCKET", "_LOG")) or name == "HOME":
            path = Path(value)
            if path in forbidden or not under(path, root):
                raise Refusal("production or non-run path")
    if len(str(root / "attach.sock").encode()) > 100:
        raise Refusal("run path too long for the disposable Unix sockets")


def prepare(root, pins):
    """Create only a fresh run tree; pin validation reads files, executes none."""
    root = Path(os.path.abspath(root))
    check_root_location(root)
    if root.exists() or root.is_symlink():
        raise Refusal("run root already exists; choose a fresh run id")
    disk_backed(root)
    root.mkdir(mode=0o700, parents=True)
    for relative in ("work", "work/.git", "home", "codex-home", "bin", "state", "raw", "xdg",
                     "xdg/config", "xdg/data", "xdg/state", "xdg/cache", "xdg/runtime"):
        (root / relative).mkdir(mode=0o700)
    (root / "work/.dot-agent-deck.toml").write_text(FIXTURE)
    # A minimal empty git repository, without invoking a user's git hooks/config.
    (root / "work/.git/objects").mkdir()
    (root / "work/.git/refs").mkdir()
    (root / "work/.git/HEAD").write_text("ref: refs/heads/hfrx\n")
    (root / "work/.git/config").write_text("[core]\n\trepositoryformatversion = 0\n\tbare = false\n")
    for name, entry in pins.items():
        if not re.fullmatch(r"[a-z0-9][a-z0-9.-]*", name):
            raise Refusal("invalid fixture binary name")
        source = Path(entry["path"])
        if not source.is_absolute() or not source.is_file() or not os.access(source, os.X_OK):
            raise Refusal(f"missing absolute executable pin: {name}")
        with source.open("rb") as binary:
            if hashlib.file_digest(binary, "sha256").hexdigest() != entry["sha256"]:
                raise Refusal(f"binary hash mismatch: {name}")
        (root / "bin" / name).symlink_to(source)
    write_json(root / "pins.json", pins)
    env = fixture_env(root)
    preflight(root, env, production_sockets(os.environ))
    write_json(root / "plan.json", plan())
    write_json(root / "environment.json", env)
    return root


def plan():
    return {"rows": ROWS, "editors": EDITORS,
            "M-FIRST": {field: "needs-implementation: Q35-07 observer" for field in FIRST_FIELDS},
            "policy": "Hold (X3 only: reviewed disposable e2e Authority seam)",
            "network": "unshare user+net; unavailable -> detection-only",
            "host_boundary": "network namespaces do not isolate host paths or host AF_UNIX",
            "code_rows": "UNVERIFIED: no run-owned display",
            "claude_rows": "outside credential-free H-FRX; separate lane-2 authorization"}


class Children:
    """Linux subreaper; signals use captured pidfds, not process-name matching."""

    def __init__(self, root, env):
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise Refusal("Linux pidfd support required; macOS execution is UNVERIFIED")
        if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:
            raise Refusal("PR_SET_CHILD_SUBREAPER failed")
        self.root, self.env = root, env
        self.handles = {}
        self.processes = []
        self.containers = []
        self.handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGTERM, signal.SIGINT)}
        for sig in self.handlers:
            signal.signal(sig, self.interrupted)

    @staticmethod
    def interrupted(sig, _frame):
        raise StopRequested(f"stopping captured children after signal {sig}")

    def capture(self, pid):
        if pid in self.handles and selectors_ready(self.handles[pid]):
            os.close(self.handles.pop(pid))
        if pid not in self.handles:
            try:
                self.handles[pid] = os.pidfd_open(pid)
            except ProcessLookupError:
                pass

    def spawn(self, argv, **kwargs):
        proc = subprocess.Popen(argv, cwd=self.root / "work", env=self.env, **kwargs)
        self.processes.append(proc)
        self.capture(proc.pid)
        return proc

    def adopted(self):
        for file in Path(f"/proc/{os.getpid()}/task").glob("*/children"):
            try:
                for pid in file.read_text().split():
                    self.capture(int(pid))
            except FileNotFoundError:
                continue

    def send(self, sig):
        for fd in self.handles.values():
            try:
                signal.pidfd_send_signal(fd, sig)
            except ProcessLookupError:
                pass

    def alive(self):
        return [pid for pid, fd in self.handles.items() if not selectors_ready(fd)]

    def cleanup(self):
        self.adopted()
        self.send(signal.SIGTERM)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            self.adopted()
            self.send(signal.SIGTERM)
            if not self.alive():
                break
            time.sleep(0.02)
        self.adopted()
        self.send(signal.SIGKILL)
        deadline = time.monotonic() + 1
        while time.monotonic() < deadline:
            self.adopted()
            self.send(signal.SIGKILL)
            if not self.alive():
                break
            time.sleep(0.02)
        survivors = self.alive()
        for proc in self.processes:
            proc.poll()
        # Container names are generated here, never accepted from the operator.
        for name in self.containers:
            proc = self.spawn([str(self.root / "bin/podman"), "rm", "-f", name],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                proc.wait(timeout=3)
                if proc.returncode:
                    survivors.append(f"container:{name}")
            except subprocess.TimeoutExpired:
                self.send(signal.SIGKILL)
                survivors.append(f"container:{name}")
        self.adopted()
        self.send(signal.SIGKILL)
        survivors.extend(pid for pid in self.alive() if pid not in survivors)
        for fd in self.handles.values():
            os.close(fd)
        while True:
            try:
                if os.waitpid(-1, os.WNOHANG)[0] == 0:
                    break
            except ChildProcessError:
                break
        for sig, handler in self.handlers.items():
            signal.signal(sig, handler)
        return survivors

    def container(self, image, argv):
        podman = str(self.root / "bin/podman")
        probe = self.spawn([podman, "image", "exists", image], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
        if probe.wait(timeout=5) != 0:
            raise Refusal("UNVERIFIED: image unavailable locally; pulls are forbidden")
        name = "hfrx-" + hashlib.sha256(str(self.root).encode()).hexdigest()[:12]
        name += f"-{len(self.containers)}"
        self.containers.append(name)
        return self.spawn([podman, "run", "--pull=never", "--network=none", "--name", name,
                           "-i", image, *argv])


def selectors_ready(fd):
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        return bool(selector.select(0))


class Capture:
    """Bounded token tail; raw bytes and chunk clocks stay on disk."""

    def __init__(self, raw, clocks):
        self.raw, self.clocks = raw, clocks
        self.offset, self.tail = 0, b""
        self.tokens = []
        self.queries = []

    def feed(self, data, mono, real):
        self.raw.write(data)
        self.clocks.write(json.dumps({"offset": self.offset, "length": len(data),
                                     "monotonic_ns": mono, "realtime_ns": real}) + "\n")
        joined = self.tail + data
        start = self.offset - len(self.tail)
        for label, needle in (("enable", ENABLE), ("disable", DISABLE), ("frame", FRAME),
                              ("cpr", b"\x1b[6n")):
            pos = joined.find(needle)
            while pos >= 0:
                if start + pos + len(needle) > self.offset:
                    item = {"kind": label, "offset": start + pos,
                            "end_offset": start + pos + len(needle),
                            "monotonic_ns": mono, "realtime_ns": real}
                    if label == "cpr":
                        self.queries.append(item)
                    else:
                        self.tokens.append(item)
                pos = joined.find(needle, pos + 1)
        self.offset += len(data)
        self.tail = joined[-16:]


def arrival_result(tokens, after_offset):
    enable = next((t for t in tokens if t["kind"] == "enable" and t["offset"] >= after_offset), None)
    frame = next((t for t in tokens if enable and t["kind"] == "frame"
                  and t["offset"] >= enable["end_offset"]), None)
    if not enable or not frame:
        return {"status": "UNVERIFIED", "reason": "missing enable/frame pair"}
    return {"status": "observed", "label": ROWS["X2-arrival"]["label"],
            "enable_offset": enable["offset"], "frame_offset": frame["offset"],
            "arrival_interval_ns": frame["monotonic_ns"] - enable["monotonic_ns"]}


def decode_trace_string(value):
    return ast.literal_eval('b"' + value + '"')


def check_connects(files, root, port):
    """Detection, including failed attempts. Unknown/truncated endpoints stop a series."""
    if not files:
        raise Refusal("UNVERIFIED containment: no connect trace")
    for file in files:
        for line in file.read_text().splitlines():
            if "connect(" not in line:
                continue
            if "AF_INET," in line:
                match = re.search(r'sin_port=htons\((\d+)\).*inet_addr\("([^"\n]*)"\)', line)
                if match and int(match[1]) == port and decode_trace_string(match[2]) == b"127.0.0.1":
                    continue
            if "AF_UNIX," in line:
                match = re.search(r'sun_path="([^"\n]*)"', line)
                if match:
                    value = decode_trace_string(match[1]).decode()
                    if value.startswith("/") and under(Path(os.path.normpath(value)), root):
                        continue
            raise Refusal("UNVERIFIED containment: unexpected or undecodable connect endpoint")


def syscall_pairs(files):
    pairs = []
    for file in files:
        tail, enable_time, editor_cycle = b"", None, False
        for line in file.read_text().splitlines():
            match = re.match(r"(\d+\.\d+)\s+(.*)", line)
            if not match:
                continue
            at, call = float(match[1]), match[2]
            write = re.match(r"write(v)?\([12],", call)
            returned = re.search(r"\)\s+=\s+(\d+)\s*$", call)
            if write and returned:
                chunks = re.findall(r'"((?:\\.|[^"\\])*)"', call)
                data = b"".join(decode_trace_string(chunk) for chunk in chunks)[:int(returned[1])]
                if DISABLE in tail + data:
                    editor_cycle = True
                    enable_time = None
                if editor_cycle and ENABLE in tail + data:
                    enable_time = at
                tail = (tail + data)[-7:]
            if re.search(r"ioctl\(0, TCFLSH, (TCIFLUSH|0)\)\s+=\s+0", call) and enable_time is not None:
                pairs.append({"pid": file.suffix[1:], "write_realtime": enable_time,
                              "flush_realtime": at, "interval_ns": round((at - enable_time) * 1e9)})
                enable_time = None
                editor_cycle = False
    return pairs


def cross_clock_order(event_real_ns, syscall_real_ns):
    delta = syscall_real_ns - event_real_ns
    return "unknown-within-1ms" if abs(delta) <= 1_000_000 else ("before" if delta > 0 else "after")


def distribution(values):
    values = sorted(values)
    if not values:
        return {"count": 0, "p50_ns": None, "p99_ns": None, "max_ns": None}
    return {"count": len(values), "p50_ns": values[(len(values) * 50 + 99) // 100 - 1],
            "p99_ns": values[(len(values) * 99 + 99) // 100 - 1], "max_ns": values[-1]}


def x1_summary(records):
    """Post-frontier observations, not a census of future writers.

    The future driver supplies raw bytes only after the exact frontier offset,
    plus a silent 10-second window. An absent frontier supplies no negative.
    """
    result = {key: 0 for key in ("runs", "unavailable", "missing_frontier", "late_frontier",
              "unknown_report", "qualified_windows", "foreign_bytes", "foreign_frame",
              "foreign_enable", "orphan_sighup", "orphan_sigcont")}
    latency = []
    for row in records:
        result["runs"] += 1
        if not row["available"]:
            result["unavailable"] += 1
            continue
        if row["report"] not in {"Clean", "Survivors"}:
            result["unknown_report"] += 1
        frontier = row["frontier"]
        if frontier not in {"taken", "late", "missing"}:
            raise Refusal("X1 unknown frontier status")
        if frontier != "taken":
            result[f"{frontier}_frontier"] += 1
            continue
        if row["window_ns"] < 10_000_000_000 or not row["standin_silent"]:
            raise Refusal("X1 requires a silent, full 10-second post-frontier observation")
        result["qualified_windows"] += 1
        data = row["post_frontier_bytes"]
        result["foreign_bytes"] += bool(data)
        result["foreign_frame"] += FRAME in data
        result["foreign_enable"] += ENABLE in data
        result["orphan_sighup"] += bool(row["orphan_sighup"])
        result["orphan_sigcont"] += bool(row["orphan_sigcont"])
        latency.append(row["frontier_latency_ns"])
    result["frontier_latency_ns"] = latency
    n = result["qualified_windows"]
    result["zero_in_n"] = (f"none in {n} for this setup, version and harness; "
                           f"rule-of-three upper bound approximately {min(1, 3 / n):.6g}"
                           if n and not result["foreign_bytes"] else None)
    return result


def export_scrubbed(root, destination):
    """Scan first, then copy. No raw file is exported if scanning fails."""
    root, destination = Path(root).absolute(), Path(destination).absolute()
    preflight(root, fixture_env(root), production_sockets(os.environ))
    check_root_location(destination)
    if destination.exists() or under(destination.absolute(), root.absolute()):
        raise Refusal("export destination must be fresh and outside the run")
    patterns = [rb"\bsk-[A-Za-z0-9_-]{8,}", rb"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
                rb"(?i)bearer\s+[A-Za-z0-9._-]{12,}",
                rb'(?i)(?:api[_-]?key|access[_-]?token|refresh[_-]?token)\s*[\"\x27]?\s*[:=]\s*[\"\x27]?[A-Za-z0-9._-]{8,}']
    files = []
    for path in root.rglob("*"):
        if path.is_symlink():
            if under(path, root / "bin"):
                continue
            raise Refusal("export refuses symlinks")
        if not path.is_file():
            continue
        # No credentials should exist, but don't silently exclude one if they do.
        if path.stat().st_size > 128 * 1024 * 1024:
            raise Refusal("export file too large; review locally")
        data = path.read_bytes()
        # strace -xx encodes even key-shaped ASCII. Scan decoded quoted strings
        # too, so the default trace format does not defeat the scrubber.
        decoded = []
        for literal in re.findall(rb'"((?:\\.|[^"\\])*)"', data):
            try:
                decoded.append(decode_trace_string(literal.decode("ascii")))
            except (ValueError, SyntaxError, UnicodeDecodeError):
                continue
        scan = data + b"\n" + b"\n".join(decoded)
        clean = re.sub(rb"(?<![A-Za-z0-9_-])" + FAKE_KEY.encode() + rb"(?![A-Za-z0-9_-])", b"FIXTURE", scan)
        if any(re.search(pattern, clean) for pattern in patterns):
            raise Refusal("export refused: key-shaped string (inspect locally; content withheld)")
        files.append((path.relative_to(root), data))
    destination.mkdir(mode=0o700, parents=True)
    for relative, data in files:
        target = destination / relative
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with target.open("xb") as out:
            out.write(data)
    return len(files)


def first_summary(records):
    """Offline observer contract check + KM; no per-episode history retained here."""
    totals, durations, causes, resolutions = 0, {}, {}, {}
    platform = None
    for row in records:
        if totals >= 100_000:
            raise Refusal("M-FIRST record cap reached; split the observation period")
        if any(field not in row for field in FIRST_FIELDS):
            raise Refusal("M-FIRST missing required observer field")
        if row["platform"] not in ("Linux", "macOS") or row["cause"] not in CAUSES | {None}:
            raise Refusal("M-FIRST unknown platform/cause")
        if platform is not None and row["platform"] != platform:
            raise Refusal("M-FIRST analyze platforms separately")
        platform = row["platform"]
        duration, stop = row["duration_ns"], row["first_stop_ns"]
        if duration < 0 or (stop is not None and not 0 <= stop <= duration):
            raise Refusal("M-FIRST invalid duration")
        if (stop is None) != (row["cause"] is None):
            raise Refusal("M-FIRST censored/stopping cause mismatch")
        pending = row["pending_resolutions"]
        if set(pending) != {"Armed", "Unarmed", "Tool", "ExitNoDecision"}:
            raise Refusal("M-FIRST unknown Pending resolution")
        if any(n < 0 for n in pending.values()) or sum(pending.values()) != row["pending_count"]:
            raise Refusal("M-FIRST Pending resolution count mismatch")
        if not 0 <= row["pending_median_ns"] <= row["pending_max_ns"]:
            raise Refusal("M-FIRST invalid Pending duration aggregate")
        end = duration if stop is None else stop
        cell = durations.setdefault(end, [0, 0])
        cell[1 if stop is None else 0] += 1
        causes[row["cause"] or "censored"] = causes.get(row["cause"] or "censored", 0) + 1
        for key, count in pending.items():
            resolutions[key] = resolutions.get(key, 0) + count
        totals += 1
    at_risk, survival, median, km = totals, 1.0, None, []
    for at, (events, censored) in sorted(durations.items()):
        survival *= 1 - events / at_risk
        km.append({"time_ns": at, "survival": survival, "events": events, "censored": censored})
        if median is None and survival <= 0.5:
            median = at
        at_risk -= events + censored
    return {"generations": totals, "median_ns": median, "km": km, "causes": causes,
            "pending_resolutions": resolutions, "status": "observer data, not measured by H-FRX self-check"}


class MockProvider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[],"models":[]}')

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        if not 0 <= size <= 1024 * 1024:
            self.send_error(413)
            return
        self.rfile.read(size)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        response = {"type": "response.completed", "response": {"id": "hfrx", "object": "response",
                    "status": "completed", "output": [], "usage": {"input_tokens": 0, "output_tokens": 0}}}
        self.wfile.write(("event: response.completed\ndata: " + json.dumps(response) + "\n\n").encode())


def pins_recheck(root):
    pins = json.loads((root / "pins.json").read_text())
    for name, entry in pins.items():
        path = root / "bin" / name
        with path.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() != entry["sha256"]:
                raise Refusal(f"binary changed since dry run: {name}")
    return pins


def config_text(port, root):
    return (f'model = "gpt-5.4-mini"\nmodel_provider = "hfrx"\n'
            'check_for_update_on_startup = false\n'
            '[analytics]\nenabled = false\n[feedback]\nenabled = false\n'
            f'[projects.{json.dumps(str(root / "work"))}]\ntrust_level = "trusted"\n'
            '[model_providers.hfrx]\nname = "H-FRX loopback fixture"\n'
            f'base_url = "http://127.0.0.1:{port}/v1"\nwire_api = "responses"\n'
            f'experimental_bearer_token = "{FAKE_KEY}"\nrequires_openai_auth = false\n'
            'supports_websockets = false\n')


def x2_sample(root, children, version, sample, method):
    prefix = root / "raw" / f"{version}-{method}-{sample:03d}"
    notice = root / "editor-notice"
    notice.unlink(missing_ok=True)
    script = Path(__file__).resolve()
    # Per-command environment adds only the run-owned editor invocation.
    editor = shlex.join([str(root / "bin/python3"), str(script), "_editor", str(root)])
    argv = [str(root / "bin/strace"), "-f", "-ff", "-ttt", "-xx", "-s", "65536",
            "-e", "trace=" + ("connect,write,writev,ioctl" if method == "instrumented" else "connect"),
            "-o", str(prefix) + ".trace", str(root / "bin" / f"codex-{version}"), "--no-alt-screen"]
    master, slave = pty.openpty()
    import fcntl
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    def controlling_tty():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    # No implicit shell, no print-mode shortcut, no inherited credential env.
    previous = children.env
    children.env = {**previous, "VISUAL": editor, "EDITOR": editor}
    try:
        proc = children.spawn(argv, stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_tty)
    finally:
        children.env = previous
        os.close(slave)
    os.set_blocking(master, False)
    try:
        with Path(str(prefix) + ".master").open("xb") as raw, Path(str(prefix) + ".clocks.jsonl").open("x") as clocks:
            capture = Capture(raw, clocks)
            trigger_offset, disabled_offset = None, None
            deadline = time.monotonic() + 30
            with selectors.DefaultSelector() as selector:
                selector.register(master, selectors.EVENT_READ)
                while time.monotonic() < deadline and proc.poll() is None:
                    if not selector.select(0.05):
                        continue
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not data:
                        break
                    capture.feed(data, time.monotonic_ns(), time.time_ns())
                    while capture.queries:
                        capture.queries.pop()
                        os.write(master, b"\x1b[1;1R")  # disclosed test-owned CPR answerer
                    # A completed first render is the ready signal, not a timed sleep.
                    if trigger_offset is None and any(t["kind"] == "frame" for t in capture.tokens):
                        trigger_offset = capture.offset
                        os.write(master, b"\x07")
                    if trigger_offset is not None and disabled_offset is None:
                        disabled = next((t for t in capture.tokens if t["kind"] == "disable"
                                         and t["offset"] >= trigger_offset), None)
                        if disabled:
                            disabled_offset = disabled["end_offset"]
                    if notice.exists() and disabled_offset is not None:
                        result = arrival_result(capture.tokens, disabled_offset)
                        if result["status"] == "observed":
                            result.update({"method": method, "release": version,
                                           "editor_invoked": True, "CPR": "test-owned fixed 1;1 answerer"})
                            return result
            return {"status": "UNVERIFIED", "reason": "interactive editor cycle did not finish",
                    "editor_invoked": notice.exists(), "method": method, "release": version}
    finally:
        os.close(master)
        if proc.poll() is None:
            signal.pidfd_send_signal(children.handles[proc.pid], signal.SIGTERM)
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            signal.pidfd_send_signal(children.handles[proc.pid], signal.SIGKILL)
            proc.wait(timeout=2)


def run_x2_inner(root, method, samples, network):
    env = fixture_env(root)
    preflight(root, env, production_sockets({}))
    pins = pins_recheck(root)
    required = {"python3", "strace", *(f"codex-{version}" for version in VERSIONS)}
    if not required <= pins.keys():
        raise Refusal("missing required pins for X2")
    children = Children(root, env)
    provider = None
    try:
        if network == "network-namespace":
            proc = children.spawn([str(root / "bin/ip"), "link", "set", "lo", "up"])
            if proc.wait(timeout=3) != 0:
                raise Refusal("loopback could not be brought up")
        provider = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MockProvider)
        port = provider.server_address[1]
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        (root / "codex-home/config.toml").write_text(config_text(port, root))
        results = []
        for version in VERSIONS:
            for sample in range(samples):
                result = x2_sample(root, children, version, sample, method)
                traces = sorted((root / "raw").glob(f"{version}-{method}-{sample:03d}.trace.*"))
                check_connects(traces, root, port)
                result["containment"] = network
                result["endpoint_detection"] = "no unexpected connect found in retained trace"
                result["frontier_to_flush"] = "needs-implementation: Q35-20a"
                if method == "instrumented":
                    result["syscall_pairs"] = syscall_pairs(traces)
                    result["timing_label"] = ROWS["X2-instrumented"]["label"]
                    if not result["syscall_pairs"]:
                        result["status"] = "UNVERIFIED"
                write_json(root / "raw" / f"{version}-{method}-{sample:03d}.json", result)
                results.append(result)
                if result["status"] != "observed":
                    raise Refusal("UNVERIFIED X2 cycle; stop series and inspect run-owned artifacts")
        summary = {}
        for version in VERSIONS:
            rows = [row for row in results if row["release"] == version]
            summary[version] = {
                "master_observed_arrival": distribution(row["arrival_interval_ns"] for row in rows),
                "instrumented_write_to_flush": distribution(pair["interval_ns"] for row in rows
                                                             for pair in row.get("syscall_pairs", []))}
        write_json(root / f"X2-{method}.json", {"results": results, "distribution": summary,
                   "host_boundary": plan()["host_boundary"], "policy": "Hold; Codex-only, no daemon"})
    finally:
        if provider:
            provider.shutdown()
            provider.server_close()
        write_json(root / f"cleanup-{method}.json", {"survivors": children.cleanup()})


def run_x2(args):
    root = Path(args.root).absolute()
    preflight(root, fixture_env(root), production_sockets(os.environ))
    pins = pins_recheck(root)
    # Require an immutable reviewed revision identifier, not an implied permission
    # from dry-run. The current task does NOT authorize invoking this command.
    if not re.fullmatch(r"[0-9a-f]{40}", args.reviewed_commit):
        raise Refusal("run requires the full separately reviewed harness commit")
    children = Children(root, fixture_env(root))
    try:
        network = "detection-only"
        if {"unshare", "ip", "python3", "true"} <= pins.keys():
            prefix = [str(root / "bin/unshare"), "--user", "--net", "--map-root-user", "--fork"]
            probe = children.spawn([*prefix, str(root / "bin/true")],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if probe.wait(timeout=3) == 0:
                network = "network-namespace"
        write_json(root / "run-authorization.json", {"reviewed_commit": args.reviewed_commit,
                   "network": network, "host_boundary": plan()["host_boundary"]})
        argv = [str(root / "bin/python3"), str(Path(__file__).resolve()), "_x2",
                str(root), args.method, str(args.samples), network]
        if network == "network-namespace":
            argv = [*prefix, *argv]
        proc = children.spawn(argv)
        if proc.wait() != 0:
            raise Refusal("X2 child failed; retain artifacts, do not resume this run id")
    finally:
        write_json(root / "cleanup-supervisor.json", {"survivors": children.cleanup()})


def main():
    # Private modes are used by run-x2, never by dry-run/self-tests.
    if len(sys.argv) > 1 and sys.argv[1] == "_editor":
        root = Path(sys.argv[2])
        if root.is_absolute() and (root / "work/.dot-agent-deck.toml").read_text() == FIXTURE:
            (root / "editor-notice").write_text("trivial fixture editor invoked\n")
            return
        raise Refusal("invalid editor fixture")
    if len(sys.argv) > 1 and sys.argv[1] == "_x2":
        run_x2_inner(Path(sys.argv[2]), sys.argv[3], int(sys.argv[4]), sys.argv[5])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("plan")
    dry = sub.add_parser("dry-run", help="create fixture and refuse unsafe inputs; start no child")
    dry.add_argument("--root", required=True)
    dry.add_argument("--pins", type=Path, required=True)
    run = sub.add_parser("run-x2", help="REAL CODEX; requires separate authorization after review")
    run.add_argument("--root", required=True)
    run.add_argument("--method", choices=["arrival", "instrumented"], required=True)
    run.add_argument("--samples", type=int, choices=range(1, 101), default=100)
    run.add_argument("--reviewed-commit", required=True)
    export = sub.add_parser("export")
    export.add_argument("root", type=Path)
    export.add_argument("destination", type=Path)
    first = sub.add_parser("summarize-first")
    first.add_argument("jsonl", type=Path)
    args = parser.parse_args()
    if args.command == "plan":
        print(json.dumps(plan(), indent=2))
    elif args.command == "dry-run":
        root = prepare(args.root, json.loads(args.pins.read_text()))
        write_json(root / "dry-run.json", {"status": "preflight passed", "agents_started": 0,
                   "subprocesses_started": 0, "network_probed": False, "measurements": 0})
        print("DRY RUN: preflight passed; stopped before namespace, mock, daemon, editor or agent startup")
    elif args.command == "run-x2":
        run_x2(args)
    elif args.command == "export":
        print(f"exported {export_scrubbed(args.root, args.destination)} scrub-checked files")
    elif args.command == "summarize-first":
        with args.jsonl.open() as source:
            print(json.dumps(first_summary(json.loads(line) for line in source), indent=2))


if __name__ == "__main__":
    try:
        main()
    except (Refusal, StopRequested, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        # Errors deliberately omit external subprocess output/configuration content.
        print(f"H-FRX REFUSED: {error}", file=sys.stderr)
        sys.exit(2)
