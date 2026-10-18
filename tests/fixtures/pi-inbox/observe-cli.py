#!/usr/bin/env python3
"""Observe scratch CLI pipes, then exec the real CLI in the original PID.

The parent of `inbox wait` identifies Pi's component instance. A subprocess
proxy changes that identity and breaks announce, fetch and watcher resume.
Only the pipe observer forks; the CLI retains both its PID and its Pi parent.
"""
import json
import os
from pathlib import Path
import select
import sys
import time

configuration = json.loads(Path(__file__).with_name("observer-config.json").read_text())
argv = sys.argv[1:]
watcher = os.getpid()
instance = os.getppid()
wait = argv[:2] == ["inbox", "wait"] and "--help" not in argv
if wait and os.environ.get("DECK_TEST_DOORBELL_ONLY") == "1":
    argv.append("--doorbell-only")


def option(name):
    return argv[argv.index(name) + 1] if name in argv else None


def record(**data):
    value = dict(ns=str(time.monotonic_ns()), observer=os.getpid(), child=watcher,
                 instance=instance, pane=os.environ.get("DOT_AGENT_DECK_PANE_ID"),
                 agent_id=os.environ.get("DOT_AGENT_DECK_AGENT_ID"), **data)
    encoded = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    fd = os.open(configuration["timeline"], os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        os.write(fd, encoded)
    finally:
        os.close(fd)


def reference(value):
    return {key: value[key] for key in ("id", "a", "e", "d", "answer") if key in value}


def key(value):
    return value["id"], value["a"], value["e"]


def pump(input_write, output_read):
    rx = None
    held = {}
    handed = set()
    committed = set()
    pending = {0: b"", output_read: b""}

    def observe(direction, line):
        nonlocal rx, held
        try:
            value = json.loads(line)
        except (ValueError, UnicodeDecodeError):
            return
        if not isinstance(value, dict):
            return
        data = {}
        # Positive allowlist: no caps, capability tokens, message text or env.
        for stage in ("messages", "handed", "committed", "not_handed", "resume",
                      "resume_answers", "rebound", "revoke", "already_handed", "revoked_ack"):
            if isinstance(value.get(stage), list):
                data[stage] = [reference(v) for v in value[stage]]
        if "registered" in value:
            rx = value["registered"]["rx"]
            data["registered"] = {"rx": rx}
        handed.update(key(v) for v in value.get("handed", []))
        committed.update(key(v) for v in value.get("committed", []))
        if "possess" in value:
            possession = value["possess"]
            next_held = {key(v): reference(v) for v in possession["held"]}
            data["possess"] = dict(seq=possession["seq"], seen=possession["seen"], held=list(next_held.values()))
            # lost() is possession removal, not a fabricated lost control.
            released = [v for k, v in held.items() if k not in next_held and k in handed and k not in committed]
            if released:
                data["uncommitted_release"] = released
            held = next_held
        if "may" in value:
            data["may"] = dict(reference(value["may"]), q=value["may"]["q"])
        if "grant" in value:
            data["grant"] = {k: value["grant"][k] for k in ("q", "ok")}
        if data:
            record(direction=direction, rx=rx, **data)

    readers = [0, output_read]
    while readers:
        # If the exec'd watcher died, exit promptly even if Pi still holds
        # its old stdin open. No process-name signalling or parent signalling.
        if os.getppid() != watcher and 0 in readers:
            readers.remove(0)
            os.close(input_write)
            # Drain the exec'd CLI's final stdout through EOF. In particular,
            # a short fetch can exit before this observer gets CPU time.
        ready, _, _ = select.select(readers, [], [], 0.1)
        for fd in ready:
            chunk = os.read(fd, 65536)
            if not chunk:
                if fd == 0:
                    readers.remove(fd)
                    os.close(input_write)
                    continue
                return
            pending[fd] += chunk
            while b"\n" in pending[fd]:
                line, pending[fd] = pending[fd].split(b"\n", 1)
                observe("stdin" if fd == 0 else "stdout", line)
            destination = input_write if fd == 0 else 1
            while chunk:
                chunk = chunk[os.write(destination, chunk):]


record(direction="spawn", verb=argv[:2], slot=option("--agent"),
       integration=option("--adapter-integration"), resumed="--resumed" in argv,
       doorbellOnly="--doorbell-only" in argv, doorbell=option("--doorbell"),
       fetchRx=option("--rx"), event=option("--type") if argv[:1] == ["agent-event"] else None)
if wait or argv[:2] == ["inbox", "fetch"]:
    input_read, input_write = os.pipe()
    output_read, output_write = os.pipe()
    observer = os.fork()
    if observer == 0:
        os.close(input_read)
        os.close(output_write)
        try:
            pump(input_write, output_read)
        except BrokenPipeError:
            pass  # The genuine CLI or Pi closed its pipe.
        except BaseException as error:
            record(direction="error", code=type(error).__name__)
            os._exit(1)
        os._exit(0)
    os.close(input_write)
    os.close(output_read)
    os.dup2(input_read, 0)
    os.dup2(output_write, 1)
    os.close(input_read)
    os.close(output_write)
# Native status/announce/probe invocations need no pipe observer. In every
# case exec preserves the real CLI's exit status and its parent identity.
os.execv(configuration["real"], [configuration["real"], *argv])
