#!/usr/bin/env bash
# PRD 2557 milestone 1 (mock-ups only, never merged): capture this box's live
# deck facts, read-only, into src/data/fleetMock.live.json for the fleet view
# mock-up scene. Reads `dot-agent-deck daemon status`, `dot-agent-deck card
# list` and `gh pr/issue/run view`; writes nothing anywhere else. Home paths are
# replaced with `~` because the images are posted on a public issue.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${FLEET_MOCK_LIVE_OUT:-$HERE/src/data/fleetMock.live.json}"
DAD="${DAD_BIN:-$HOME/.local/bin/dot-agent-deck}"
REPO=vfarcic/dot-agent-deck
PRS=(2501 2523 2527 2528 2550)
ISSUES=(2385 2399 2487 2530 2533 2555 2557 2374 2486 2395 2396)
MAIN_RUN=45990241668
# Round 3: also every pull request and issue a composed view names (FLEET_MOCK_REFS_FROM, comma-separated view files).
if [[ -n "${FLEET_MOCK_REFS_FROM:-}" ]]; then
  IFS=, read -ra views <<< "$FLEET_MOCK_REFS_FROM"
  while read -r kind n; do
    if [[ $kind == pull_request ]]; then PRS+=("$n"); else ISSUES+=("$n"); fi
  done < <(python3 -I -c 'import json,sys
for path in sys.argv[1:]:
    for m in json.load(open(path)):
        for c in m.get("updateComponents", {}).get("components", []):
            if c.get("component") == "DeckRef": print(c.get("kind"), int(c.get("number")))' "${views[@]}" | sort -u)
  mapfile -t PRS < <(printf '%s\n' "${PRS[@]}" | sort -un)
  mapfile -t ISSUES < <(printf '%s\n' "${ISSUES[@]}" | sort -un)
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

"$DAD" daemon status > "$tmp/status.tsv"
"$DAD" card list > "$tmp/cards.json"
for n in "${PRS[@]}"; do
  gh pr view "$n" --repo "$REPO" --json number,title,state,reviewDecision,mergeStateStatus,labels,author,mergedAt,mergeCommit,statusCheckRollup > "$tmp/pr-$n.json" 2>/dev/null || { echo "not a pull request: #$n" >&2; rm -f "$tmp/pr-$n.json"; }
done
for n in "${ISSUES[@]}"; do
  gh issue view "$n" --repo "$REPO" --json number,title,state,labels > "$tmp/issue-$n.json" 2>/dev/null || { echo "not an issue: #$n" >&2; rm -f "$tmp/issue-$n.json"; }
done
gh run view "$MAIN_RUN" --repo "$REPO" --json databaseId,conclusion,status,headSha > "$tmp/run.json"

python3 -I - "$tmp" "$OUT" "$HOME" <<'PY'
import json, sys, os, glob, datetime
tmp, out, home = sys.argv[1], sys.argv[2], sys.argv[3]

def scrub(value):
    if isinstance(value, str):
        return value.replace(home, "~")
    if isinstance(value, list):
        return [scrub(v) for v in value]
    if isinstance(value, dict):
        return {k: scrub(v) for k, v in value.items()}
    return value

rows = []
with open(os.path.join(tmp, "status.tsv")) as f:
    header = f.readline().rstrip("\n").split("\t")
    for line in f:
        if line.strip():
            rows.append(dict(zip(header, line.rstrip("\n").split("\t"))))

def checks(rollup):
    counts = {"passing": 0, "failing": 0, "pending": 0, "skipped": 0}
    failing = []
    for c in rollup or []:
        s = (c.get("conclusion") or c.get("state") or "").upper()
        name = c.get("name") or c.get("context")
        if s in ("SUCCESS", "NEUTRAL"):
            counts["passing"] += 1
        elif s in ("FAILURE", "ERROR", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE"):
            counts["failing"] += 1
            failing.append(name)
        elif s == "SKIPPED":
            counts["skipped"] += 1
        else:
            counts["pending"] += 1
    state = "failing" if counts["failing"] else "pending" if counts["pending"] else "passing"
    return {"state": state, **counts}, failing

prs = {}
for path in sorted(glob.glob(os.path.join(tmp, "pr-*.json"))):
    pr = json.load(open(path))
    ci, failing = checks(pr.get("statusCheckRollup"))
    prs[str(pr["number"])] = {
        "number": pr["number"], "title": pr["title"], "state": pr["state"].lower(),
        "review": {"APPROVED": "approved", "CHANGES_REQUESTED": "changes_requested", "REVIEW_REQUIRED": "review_required"}.get(pr.get("reviewDecision") or "", "review_required"),
        "mergeable": (pr.get("mergeStateStatus") or "UNKNOWN").lower(),
        "labels": [l["name"] for l in pr.get("labels", [])],
        "author": pr["author"]["login"], "mergedAt": pr.get("mergedAt"),
        "mergeCommit": (pr.get("mergeCommit") or {}).get("oid"),
        "ci": ci, "failing": failing,
    }
issues = {}
for path in sorted(glob.glob(os.path.join(tmp, "issue-*.json"))):
    issue = json.load(open(path))
    issues[str(issue["number"])] = {"number": issue["number"], "title": issue["title"], "state": issue["state"].lower(), "labels": [l["name"] for l in issue.get("labels", [])]}

doc = {
    "captured_at": datetime.datetime.now(datetime.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "daemon_status": rows,
    "cards": json.load(open(os.path.join(tmp, "cards.json"))),
    "pull_requests": prs,
    "issues": issues,
    "main_run": json.load(open(os.path.join(tmp, "run.json"))),
}
with open(out, "w") as f:
    json.dump(scrub(doc), f, indent=1)
    f.write("\n")
print(f"wrote {out} at {doc['captured_at']}: {len(rows)} agents, {len(prs)} PRs, {len(issues)} issues")
PY
grep -q "$HOME" "$OUT" && { echo "home path leaked into $OUT" >&2; exit 1; } || true
