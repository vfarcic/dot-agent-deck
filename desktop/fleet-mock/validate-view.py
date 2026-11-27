#!/usr/bin/env python3
"""PRD 2557 mock-up, round 3: check a fleet view against fleet-catalog.md.

Prints `ok` or one line per problem; exits non-zero on a problem. It checks
what the daemon would refuse (shape, catalog, references, placeholders), not
whether the view is any good.
"""
import json
import re
import sys

CATALOG = {
    "Text", "Column", "Row", "List", "Card", "Divider", "Icon",
    "Stat", "Chip", "Progress", "Chart", "Board", "Lane", "Timeline",
    "DeckRef", "AgentRef", "ChoicePicker", "TextField", "PromptButton",
}
CONTAINERS = {"Column", "Row", "List", "Card", "Board", "Lane"}
TONES = {"default", "muted", "info", "attention", "danger", "ok"}
ICONS = {"bell", "alert", "check", "clock", "merge", "pause", "question", "terminal", "flask", "package", "eye", "pin", "play", "x"}
REQUIRED = {
    "Text": ["text"], "Stat": ["value", "label"], "Chip": ["text"], "Progress": ["value", "max"],
    "Chart": ["kind", "data"], "Lane": ["title"], "Timeline": ["items"], "DeckRef": ["kind", "number"],
    "AgentRef": ["agent"], "ChoicePicker": ["path", "options"], "TextField": ["path"], "PromptButton": ["agent", "text"],
    "Icon": ["name"],
}


def main(path: str) -> int:
    problems = []
    try:
        messages = json.load(open(path))
    except Exception as err:  # noqa: BLE001 - any parse failure is the finding
        print(f"does not parse: {err}")
        return 1
    if not isinstance(messages, list):
        print("the top level must be a JSON array of A2UI messages")
        return 1
    components, paths, composed = [], set(), False
    for at, message in enumerate(messages):
        if message.get("version") != "1.0":
            problems.append(f"message {at}: version must be \"1.0\"")
        if "updateComponents" in message:
            components += message["updateComponents"].get("components", [])
        elif "updateDataModel" in message:
            p = message["updateDataModel"].get("path")
            composed = composed or p == "/composedAt"
            paths.add(p)
        elif "createSurface" not in message:
            problems.append(f"message {at}: not createSurface, updateComponents or updateDataModel")
    if not composed:
        problems.append("no updateDataModel for /composedAt")
    by_id = {}
    for c in components:
        cid, kind = c.get("id"), c.get("component")
        if not cid:
            problems.append(f"a component has no id: {json.dumps(c)[:80]}")
            continue
        if cid in by_id:
            problems.append(f"{cid}: id used twice")
        by_id[cid] = c
        if kind not in CATALOG:
            problems.append(f"{cid}: `{kind}` is not in the catalog")
            continue
        for prop in REQUIRED.get(kind, []):
            if prop not in c:
                problems.append(f"{cid}: {kind} needs `{prop}`")
        if "tone" in c and c["tone"] not in TONES:
            problems.append(f"{cid}: tone `{c['tone']}` is not one of {sorted(TONES)}")
        if kind == "Icon" and c.get("name") not in ICONS:
            problems.append(f"{cid}: icon `{c.get('name')}` is not in the catalog")
        if kind in ("ChoicePicker", "TextField"):
            paths.add(c.get("path"))
    if "root" not in by_id:
        problems.append("no component with id `root`")
    for cid, c in by_id.items():
        if c.get("component") in CONTAINERS:
            for child in c.get("children", []):
                if child not in by_id:
                    problems.append(f"{cid}: child `{child}` does not exist")
            if c.get("component") == "Board":
                for child in c.get("children", []):
                    if child in by_id and by_id[child].get("component") != "Lane":
                        problems.append(f"{cid}: a Board's children must be Lanes, `{child}` is not")
        if c.get("component") == "PromptButton":
            for placeholder in re.findall(r"\{(/[^}]+)\}", str(c.get("text", ""))):
                if placeholder not in paths:
                    problems.append(f"{cid}: placeholder {{{placeholder}}} has no ChoicePicker or TextField bound to it")
    reachable, stack = set(), ["root"]
    while stack:
        cid = stack.pop()
        if cid in reachable or cid not in by_id:
            continue
        reachable.add(cid)
        stack += by_id[cid].get("children", [])
    for cid in sorted(set(by_id) - reachable):
        problems.append(f"{cid}: not reachable from root, so never drawn")
    text = json.dumps(messages)
    if re.search(r"/home/|/Users/", text):
        problems.append("contains a home-directory path; the view may be posted publicly")
    for line in problems:
        print(line)
    if not problems:
        print("ok")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
