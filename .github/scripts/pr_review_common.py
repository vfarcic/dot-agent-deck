"""Shared helpers for the agent PR reviewer (.github/workflows/pr-review-batch.yml).

Kept in one module on purpose: REQUIRED_CONTEXTS, DENY_PATHS and DENY_PRECEDENCE
must not drift between selection and voting. If they did, a pull request could be
selected under one policy and voted on under another — which is not hypothetical,
since DENY_PRECEDENCE was added after exactly that happened to the truncated
`denied_paths` payload.
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

# Changes touching these need a human to READ them before they merge.
#
# Be precise about what that does and does not promise, because this comment used
# to say "the App must never vote on them" and issue #998 made that false. The App
# does vote on them now: the vote job approves with the specific reason at the top
# of the review body plus a `needs-human-eye` label when auto-merge is DISARMED,
# and declines entirely when it is armed. So the property here is "a human presses
# merge with the reason in front of them", not "the App abstains" — deliberately
# weaker, because abstaining left these pull requests waiting on a second
# maintainer and pushed the author toward an admin bypass that leaves no record at
# all. Issue #998 has the trade.
#
# The reasons differ per path, which is why DENY_PRECEDENCE below exists: the
# reviewer should not be the only reader of a change to its own powers, CLAUDE.md
# is the rubric it judges against, and a protocol change carries a cross-version
# contract obligation (CLAUDE.md rule 12) an agent cannot discharge at all.
DENY_PATHS = (
    ".github/",
    "scripts/apply-branch-protection.sh",
    "greptile.json",
    "MAINTAINERS.md",
    "CLAUDE.md",
    "src/daemon_protocol.rs",
)

# The order in which those paths' explanations take precedence, most specific
# first. TWO consumers must agree on it, which is why it lives here beside
# DENY_PATHS rather than next to the prose it selects: the vote job explains a
# pull request by the FIRST match (`deny_reason`), and the selector sorts by this
# before truncating `denied_paths`.
#
# They disagreed until #1000 review: the selector truncated to five paths in the
# GitHub files API's alphabetical order, and `src/daemon_protocol.rs` sorts LAST
# of these prefixes. A protocol change touching five `.github/` files therefore
# lost the protocol path before the vote job ever saw it, so rule 12's
# unverifiable cross-version obligation was silently downgraded to the `.github/`
# catch-all and the "Touches:" line never mentioned the protocol at all.
DENY_PRECEDENCE = (
    "src/daemon_protocol.rs",
    ".github/workflows/pr-review",
    ".github/scripts/pr_review_",
    "CLAUDE.md",
    "scripts/apply-branch-protection.sh",
    "MAINTAINERS.md",
    "greptile.json",
    ".github/",
)


def deny_sort_key(path):
    """Order a denied path by how specific its explanation is, most specific first.

    Sort by this BEFORE truncating, so the path that governs the explanation is
    never the one dropped. A path matching no prefix sorts last, and ties break on
    the path itself so the order is total and the payload is reproducible.
    """
    for index, prefix in enumerate(DENY_PRECEDENCE):
        if path.startswith(prefix):
            return (index, path)
    return (len(DENY_PRECEDENCE), path)


BAD_CONCLUSIONS = {"failure", "cancelled", "timed_out", "action_required", "stale"}

VERDICT_BLOCK = re.compile(r"```json\s*(\{.*?\})\s*```", re.DOTALL)
SCHEMA = "pr-review/v1"
VERDICTS = {"APPROVE", "REQUEST_CHANGES", "INSUFFICIENT"}

# A verdict is only a verdict if OUR workflow wrote it.
#
# Without this, the transport between the agent and the vote job is
# unauthenticated: this is a public repository, so anyone with a GitHub account
# can comment a forged pr-review/v1 block quoting the pull request's public head
# SHA, and the vote job would cast the required approval with the App
# credential. The same hole is a denial of service — one malformed block from a
# stranger raises out of the selector and kills the sweep.
#
# Two independent conditions, both required. The login can only be produced by
# an Actions run in this repository (a person cannot post as it), and the
# provenance marker is emitted by gh-aw's safe-output for this specific
# workflow, so an unrelated workflow's comment does not qualify either.
TRUSTED_VERDICT_AUTHOR = "github-actions[bot]"
VERDICT_PROVENANCE = "gh-aw-agentic-workflow:"
VERDICT_WORKFLOW_ID = "workflow_id: pr-review"


def _is_trusted_verdict_comment(comment):
    author = ((comment.get("user") or {}).get("login")) or ""
    body = comment.get("body") or ""
    return (
        author == TRUSTED_VERDICT_AUTHOR
        and VERDICT_PROVENANCE in body
        and VERDICT_WORKFLOW_ID in body
    )


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


def gh_ok(*args):
    """Run gh, return True on success. For best-effort calls where a failure
    must be visible but must not abort the caller — `gh(..., check=False)`
    returns stdout, and a failed command's stdout is empty, not distinguishable
    from a successful one that printed nothing."""
    result = subprocess.run(("gh",) + args, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        print(f"gh {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}")
        return False
    return True


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
    """Newest pr-review/v1 verdict written by OUR workflow, or None.

    Comments from anyone else are discarded before being parsed at all, so a
    forged or malformed block from an untrusted commenter can neither become
    authoritative nor raise. A malformed verdict from the trusted author still
    raises — that is a broken reviewer and must be loud, not silently skipped.
    """
    comments = gh_json(
        "api", f"repos/{repo}/issues/{pr_number}/comments", "--paginate",
        "--jq", "[.[] | {id, body, created_at, user: {login: .user.login}}]",
    ) or []
    trusted = [c for c in comments if _is_trusted_verdict_comment(c)]
    for comment in sorted(trusted, key=lambda c: c["created_at"], reverse=True):
        verdict = parse_verdict(comment.get("body"))
        if verdict is not None:
            return verdict
    return None


# The marker every no-vote notice carries, so the job can recognise its own.
#
# Both notices (an INSUFFICIENT verdict, and a deny-listed pull request whose
# auto-merge is armed) open with this, and both quote the short head SHA. Author
# plus marker plus SHA is what makes a notice identifiable as ALREADY POSTED FOR
# THIS HEAD, which is the whole question `already_noticed_at` answers.
NO_VOTE_MARKER = "**No vote cast**"


def already_reviewed_at(reviews, sha, app_login):
    """True when `app_login` has already cast a review on this exact head.

    This is the bound that was missing (issue #1050). The vote pass is selected on
    "has a current verdict" and deliberately NOT on "has not been reviewed" — that
    is what keeps a verdict from an earlier run votable and a transiently-failed
    vote retryable — so without a per-SHA guard the daily sweep re-cast the same
    approval twice a day for as long as the head sat still.

    Keyed on `commit_id` and NOT on review state, which decides two cases:

      * a push moves the head, so the old reviews carry an old `commit_id` and the
        new head is votable. That is the case that must keep working, and it is
        the reason a SHA is the right key rather than a timestamp.
      * a review the App cast and a human then DISMISSED still counts as cast.
        Re-casting it would fight the human who dismissed it, and this job is a
        second opinion rather than an authority over one.

    An empty `app_login` returns False — fail OPEN, deliberately. The failure mode
    of fail-open is a duplicate approval, which is the noise this fixes; the
    failure mode of fail-closed is a reviewer that silently approves nothing,
    which blocks every merge it was added to unblock. The caller says so loudly.
    """
    if not app_login:
        return False
    return any(
        (review.get("user") or {}).get("login") == app_login
        and review.get("commit_id") == sha
        for review in reviews or ()
    )


def already_noticed_at(comments, sha, app_login):
    """True when `app_login` has already posted a no-vote notice for this head.

    The comment-only outcomes re-posted on the same cadence and for the same
    reason as the duplicate reviews, so they take the same key: author, marker,
    and the short SHA the notice itself quotes.

    Note which branch this must NOT suppress. The armed-auto-merge notice tells
    the reader to disarm auto-merge and re-run, so it has to stay re-runnable
    INTO A VOTE. It does, because that path re-reads the armed state first and
    only consults this predicate while still armed; once disarmed the job falls
    through to voting, where no review exists at this SHA and `already_reviewed_at`
    lets it through. Fail-open on an empty login, as above.
    """
    if not app_login:
        return False
    short = (sha or "")[:8]
    if not short:
        return False
    return any(
        (comment.get("user") or {}).get("login") == app_login
        and NO_VOTE_MARKER in (comment.get("body") or "")
        and short in (comment.get("body") or "")
        for comment in comments or ()
    )


def pr_reviews(repo, pr_number):
    """Every review on a pull request, paginated.

    `--paginate` is load-bearing: the endpoint pages at 30, and a pull request
    that has collected a few rounds of Greptile and maintainer reviews pushes the
    App's own past votes off the first page — which would silently defeat the
    guard on exactly the long-lived pull requests it matters most on.
    """
    return gh_json(
        "api", f"repos/{repo}/pulls/{pr_number}/reviews", "--paginate",
        "--jq", "[.[] | {commit_id, state, user: {login: .user.login}}]",
    ) or []


def pr_comments(repo, pr_number):
    """Every issue comment on a pull request, paginated. Same paging reason."""
    return gh_json(
        "api", f"repos/{repo}/issues/{pr_number}/comments", "--paginate",
        "--jq", "[.[] | {body, user: {login: .user.login}}]",
    ) or []


def fail(message):
    print(f"::error title=Agent PR review::{message}", file=sys.stderr)
    sys.exit(1)
