"""Shared helpers for the agent PR reviewer (.github/workflows/pr-review-batch.yml).

Kept in one module on purpose: REQUIRED_CONTEXTS and DENY_PATHS must not drift
between selection and voting. If they did, a pull request could be selected under
one policy and voted on under another.
"""

import json
import re
import subprocess
import sys

# The contexts the `main-protected` ruleset requires. That ruleset is the source
# of truth; read it back with `scripts/apply-branch-protection.sh status` if you
# suspect drift. Drift in the "ruleset added one we do not list" direction would
# be fail-OPEN, so checks_green() also rejects ANY failed check on the head, not
# only these five.
REQUIRED_CONTEXTS = (
    "build",
    "build-macos",
    "build-windows",
    "security",
    "e2e-deterministic",
)

# Changes touching these need a human approval; the App must never vote on them.
# Same list as the governance paths in the design discussion: the reviewer must
# not be able to widen its own powers, and protocol changes carry a
# cross-version contract obligation (CLAUDE.md rule 12) an agent cannot discharge.
DENY_PATHS = (
    ".github/",
    "scripts/apply-branch-protection.sh",
    "greptile.json",
    "MAINTAINERS.md",
    "CLAUDE.md",
    "src/daemon_protocol.rs",
)

BAD_CONCLUSIONS = {"failure", "cancelled", "timed_out", "action_required", "stale"}

VERDICT_BLOCK = re.compile(r"```json\s*(\{.*?\})\s*```", re.DOTALL)
SCHEMA = "pr-review/v1"
VERDICTS = {"APPROVE", "REQUEST_CHANGES", "INSUFFICIENT"}


def gh(*args, check=True):
    """Run gh and return stdout. Raises on failure so nothing fails silently."""
    result = subprocess.run(
        ("gh",) + args, capture_output=True, text=True, check=False
    )
    if check and result.returncode != 0:
        raise RuntimeError(
            f"gh {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}"
        )
    return result.stdout


def gh_json(*args):
    out = gh(*args).strip()
    return json.loads(out) if out else None


def checks_green(repo, sha):
    """True when every required context succeeded and nothing else failed.

    Deliberately stricter than "the five are green": a check run that failed is
    disqualifying even if it is not required, so that adding a required context
    to the ruleset without updating REQUIRED_CONTEXTS fails closed rather than
    open. A context that produced no check run at all is NOT success.
    """
    runs = gh_json("api", f"repos/{repo}/commits/{sha}/check-runs", "--paginate",
                   "--jq", "[.check_runs[] | {name, status, conclusion}]") or []
    by_name = {}
    for run in runs:
        # Keep the newest entry per name; re-runs append.
        by_name[run["name"]] = run
    for name in REQUIRED_CONTEXTS:
        run = by_name.get(name)
        if run is None or run.get("status") != "completed":
            return False, f"required context {name!r} has not concluded"
        if run.get("conclusion") not in ("success", "skipped"):
            return False, f"required context {name!r} concluded {run.get('conclusion')!r}"
    for run in by_name.values():
        if (run.get("conclusion") or "").lower() in BAD_CONCLUSIONS:
            return False, f"check {run['name']!r} concluded {run['conclusion']!r}"
    return True, "all required contexts green, no failures"


def unresolved_threads(repo, pr_number):
    """Count unresolved review threads. Greptile's findings arrive as these.

    A pull request with open threads is not ready: `required_review_thread_resolution`
    blocks its merge regardless of approvals, so reviewing it spends tokens on a
    verdict that cannot help, and voting on it would be an approval that changes
    nothing. Only available over GraphQL.
    """
    owner, name = repo.split("/")
    query = (
        "query($o:String!,$n:String!,$p:Int!){repository(owner:$o,name:$n){"
        "pullRequest(number:$p){reviewThreads(first:100){nodes{isResolved}}}}}"
    )
    data = gh_json(
        "api", "graphql", "-f", f"query={query}", "-F", f"o={owner}", "-F", f"n={name}",
        "-F", f"p={pr_number}",
        "--jq", "[.data.repository.pullRequest.reviewThreads.nodes[]|select(.isResolved==false)]|length",
    )
    return int(data or 0)


def parse_verdict(body):
    """Extract and validate a pr-review/v1 verdict from a comment body.

    Returns the dict, or None when the body carries no verdict block. Raises
    ValueError when a block is present but malformed — a malformed verdict is a
    loud failure, never a silent skip.
    """
    matches = VERDICT_BLOCK.findall(body or "")
    if not matches:
        return None
    if len(matches) > 1:
        raise ValueError("comment contains more than one json block")
    try:
        data = json.loads(matches[0])
    except json.JSONDecodeError as exc:
        raise ValueError(f"verdict block is not valid JSON: {exc}") from exc
    if data.get("schema") != SCHEMA:
        return None
    if data.get("verdict") not in VERDICTS:
        raise ValueError(f"unknown verdict {data.get('verdict')!r}")
    if not isinstance(data.get("head_sha"), str) or len(data["head_sha"]) != 40:
        raise ValueError("verdict head_sha is missing or not a full 40-char SHA")
    return data


def latest_verdict(repo, pr_number):
    """Newest pr-review/v1 verdict on a pull request, or None."""
    comments = gh_json(
        "api", f"repos/{repo}/issues/{pr_number}/comments", "--paginate",
        "--jq", "[.[] | {id, body, created_at}]",
    ) or []
    for comment in sorted(comments, key=lambda c: c["created_at"], reverse=True):
        verdict = parse_verdict(comment.get("body"))
        if verdict is not None:
            return verdict
    return None


def fail(message):
    print(f"::error title=Agent PR review::{message}", file=sys.stderr)
    sys.exit(1)
