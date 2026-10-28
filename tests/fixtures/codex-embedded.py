#!/usr/bin/python3
"""Synthetic argv oracle; this does not implement Codex configuration merging."""
import json
import os
import sys

args = sys.argv[1:]
if "app-server" in args:
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        result = {"data": [{"cwd": os.getcwd(), "hooks": [], "warnings": [], "errors": []}]}
        if request["method"] == "initialize":
            result = {"userAgent": "synthetic-codex", "codexHome": os.environ["CODEX_HOME"],
                      "platformFamily": "unix", "platformOs": "linux"}
        print(json.dumps({"id": request["id"], "result": result}), flush=True)
    sys.exit(0)

record = os.open(os.environ["CODEX_ARGS_RECORD"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
os.write(record, (json.dumps(args) + "\n").encode())
os.close(record)
if "--no-daemon" in args:
    print("OLD_RELEASE_REJECTED_NO_DAEMON", flush=True)
    sys.exit(64)

embedded = args[:2] == ["-c", "shell_environment_policy.set={}"]
print("EMBEDDED_SUCCESS" if embedded else "SHARED_SELECTION", flush=True)
print("STDERR_SENTINEL", file=sys.stderr, flush=True)
for line in sys.stdin:
    print("INPUT_SUCCESS:" + line.rstrip("\r\n"), flush=True)
