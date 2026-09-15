"""Shared helpers for the agent PR reviewer (.github/workflows/pr-review-batch.yml).

Kept in one module on purpose: the check gate, DENY_PATHS and DENY_PRECEDENCE
must not drift between selection and voting. If they did, a pull request could be
selected under one policy and voted on under another — which is not hypothetical,
since DENY_PRECEDENCE was added after exactly that happened to the truncated
`denied_paths` payload.
"""

import json
import re
import subprocess
import sys

# The contexts the `main-protected` ruleset requires, as a LAST-RESORT FALLBACK.
#
# The live ruleset is the source of truth and `required_contexts()` below reads
# it at run time; this tuple is only what that falls back to when the API cannot
# be read. It is still a copy, so a linkage-check test pins it against
# `scripts/apply-branch-protection.sh`'s REQUIRED_CHECKS default — what keeps it
# from behaving as a third source of truth is that nothing consults it on a
# healthy run, and the path that does prints a note saying so.
#
# Verified against the live ruleset on 2026-09-15: the legacy branch-protection
# API returns nothing useful here (`required_status_checks` empty, `strict:
# null`), because enforcement moved to a ruleset. `GET /repos/{repo}/rules/
# branches/{branch}` is what reports it, and it agreed with these five and with
# the applier script.
FALLBACK_REQUIRED_CONTEXTS = (
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


def concat_json_documents(text):
    """Parse a stream of back-to-back JSON documents into one flat list.

    `gh api --paginate --jq FILTER` applies the filter to EACH PAGE and
    concatenates the results, so a request that spans pages emits several JSON
    documents rather than one and a single `json.loads` raises `Extra data`.

    The page size is **100** — `gh` appends `per_page=100` to a paginated request
    unless the caller sets it, verified with `GH_DEBUG=api`. Not 30, which is
    GitHub's default for an unpaginated call and the number this comment said
    before it was measured; the distinction decides whether a 70-file pull request
    is near the boundary or past it.

    `--slurp` is gh's own answer to this and cannot be used here: it is refused in
    combination with `--jq` ("the `--slurp` option is not supported with `--jq` or
    `--template`"), and dropping `--jq` would pull entire comment and review
    bodies through for every pull request on every sweep.

    A single-page response has exactly one document and comes back unchanged,
    which is what makes this safe at the call sites that have never yet paged.
    """
    text = (text or "").strip()
    if not text:
        return []
    decoder = json.JSONDecoder()
    items, index = [], 0
    while index < len(text):
        value, index = decoder.raw_decode(text, index)
        items.extend(value if isinstance(value, list) else [value])
        while index < len(text) and text[index] in " \t\r\n":
            index += 1
    return items


def gh_json_paginated(*args):
    """`gh_json` for a `--paginate --jq '[...]'` call. See concat_json_documents."""
    return concat_json_documents(gh(*args))


# Memoised per process. One sweep asks about a dozen pull requests and re-reading
# the ruleset for each would buy nothing, so this is two API calls per run rather
# than two per pull request.
_REQUIRED_CACHE = {}


def default_branch(repo):
    """The branch a pull request here merges into, from the API rather than a
    hardcoded "main".

    `gh`, not `gh_json`: `--jq` on a string field prints it UNQUOTED, so `main`
    is not valid JSON and `json.loads` raises on it.
    """
    return gh("api", f"repos/{repo}", "--jq", ".default_branch").strip()


def required_contexts(repo):
    """The status checks branch protection actually requires, read from the live
    ruleset.

    Derived rather than hardcoded because the enforced list has already moved
    once: the legacy branch-protection API this repository's tooling was written
    against now returns an empty `required_status_checks` with `strict: null`,
    and enforcement lives in the `main-protected` ruleset instead. Reading it per
    run means the gate does not have to be kept in step with the ruleset by hand
    — which is what makes it reasonable to gate on the required contexts ALONE
    (see classify_check_runs) instead of on every check on the head.

    Asked about the DEFAULT branch, and about nothing else, in both the selection
    and the vote job — so both ask the same question rather than each inventing
    its own, which is the drift this module exists to prevent. A pull request
    targeting some other branch is therefore judged against `main`'s contexts.
    That errs strict: no ruleset targets a feature branch here today, so deriving
    per base branch would ask LESS of a stacked pull request than of an ordinary
    one, and every pull request in this repository targets the default branch
    anyway.

    Falls back to FALLBACK_REQUIRED_CONTEXTS, loudly, when the API cannot be
    read. Fail-closed on an unreadable ruleset would mean a reviewer that
    silently selects nothing at all, which is the outage this whole gate exists
    to avoid.
    """
    if repo in _REQUIRED_CACHE:
        return _REQUIRED_CACHE[repo]
    contexts = None
    try:
        target = default_branch(repo)
        if target:
            rules = gh_json_paginated(
                "api", f"repos/{repo}/rules/branches/{target}", "--paginate",
                "--jq", "[.[] | select(.type == \"required_status_checks\")"
                        " | .parameters.required_status_checks[]? | .context]",
            )
            # Several rulesets can apply to one branch; the union is what the
            # merge button waits for. Order is fixed so the log line is stable.
            names = sorted({c for c in rules if c})
            if names:
                contexts = tuple(names)
                # Said out loud once per run: the gate is now data rather than a
                # constant, so a reader debugging a skip needs to see what it was.
                print(f"required contexts, from the {target!r} ruleset: {list(contexts)}")
    except (OSError, RuntimeError, ValueError, KeyError, TypeError) as exc:
        print(f"::warning::could not read the required contexts from the ruleset: {exc}")
    if contexts is None:
        print(
            "note: falling back to the hardcoded required contexts "
            f"{list(FALLBACK_REQUIRED_CONTEXTS)} — the live ruleset reported none"
        )
        contexts = FALLBACK_REQUIRED_CONTEXTS
    _REQUIRED_CACHE[repo] = contexts
    return contexts


def classify_check_runs(runs, required):
    """Decide whether a head is mergeable-green. Pure, so it is testable.

    Returns `(green, why, advisory_failures)`.

    Gates on the REQUIRED contexts and nothing else (issue #1086). It used to
    reject any failed check run on the head, required or not, as a fail-closed
    hedge against the ruleset requiring a context this module did not list — and
    that hedge cost more than it bought. Branch protection deliberately leaves
    the other jobs out of its required list, so they do not hold the merge
    button; but the reviewer's approval IS what satisfies the required-review
    rule, so refusing to review on an advisory red blocked the only thing that
    could unblock the merge. Measured on PR #1076: all five required contexts
    green, `devbox` red on a 504 from a third-party CDN inside someone else's
    action, and the approval withheld. The hedge is answered rather than merely
    accepted: `required_contexts()` reads the live ruleset each run, so the list
    is not one a human has to remember to update.

    A required context that produced no check run at all is still NOT success,
    and `skipped` still is — both unchanged from before, the second because a
    required job may legitimately skip on a path filter.

    Advisory failures are returned rather than swallowed, so the log says which
    red the gate deliberately ignored instead of leaving the reader to guess.
    """
    by_name = {}
    for run in runs or ():
        # Keep the newest entry per name; re-runs append.
        by_name[run["name"]] = run
    required = tuple(required)
    advisory = [
        f"{name}={run.get('conclusion')}"
        for name, run in sorted(by_name.items())
        if name not in required
        and (run.get("conclusion") or "").lower() in BAD_CONCLUSIONS
    ]
    for name in required:
        run = by_name.get(name)
        if run is None or run.get("status") != "completed":
            return False, f"required context {name!r} has not concluded", advisory
        if run.get("conclusion") not in ("success", "skipped"):
            return (
                False,
                f"required context {name!r} concluded {run.get('conclusion')!r}",
                advisory,
            )
    why = "all required contexts green"
    if advisory:
        why += f" (ignoring advisory failure(s): {', '.join(advisory)})"
    return True, why, advisory


def check_status(repo, sha):
    """`checks_green` plus the advisory failures the gate ignored."""
    runs = gh_json_paginated(
        "api", f"repos/{repo}/commits/{sha}/check-runs", "--paginate",
        "--jq", "[.check_runs[] | {name, status, conclusion}]",
    )
    return classify_check_runs(runs, required_contexts(repo))


def checks_green(repo, sha):
    """True when every context branch protection requires has succeeded.

    The vote job's own re-validation, unchanged in what it asks: it re-fetches
    the head's check runs itself rather than trusting the selection job. What
    moved under it is the policy both jobs share — required contexts only — and
    it has to move in both or a pull request is selected under one rule and
    voted on under another, which is the drift this module exists to prevent.
    The ignored advisory reds are named in the returned reason, so the vote
    job's log still shows them.
    """
    green, why, _advisory = check_status(repo, sha)
    return green, why


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
    comments = gh_json_paginated(
        "api", f"repos/{repo}/issues/{pr_number}/comments", "--paginate",
        "--jq", "[.[] | {id, body, created_at, user: {login: .user.login}}]",
    )
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


def bot_rejection_is_stale(reviews, sha):
    """True when the newest BOT changes-requested review sits on an older head.

    The reviewer could reject a pull request and then never look at it again.
    `reviewDecision` stays `CHANGES_REQUESTED` until the review is dismissed or
    superseded — GitHub's `dismiss_stale_reviews_on_push` dismisses APPROVALS
    only — and both the selector and the vote job skip on that decision, so a
    rejected pull request was permanently excluded from its own reviewer. The
    author pushed a fix and nothing came back. Measured on #1019: the rejection
    stayed pinned to `5f76cfdd` while the head moved to `47ce8906`, and it took a
    manual dismissal to unstick it.

    Keyed on BOT authorship, not on the App's login, because the selector holds
    no App credential and cannot ask who it is. That is sound here for a reason
    rather than by luck: a human's changes-requested must keep parking the pull
    request (it is someone else's homework, and re-deriving a verdict talks over
    them mid-fix), and the only other bot reviewing here posts `COMMENTED` and
    never `CHANGES_REQUESTED`. The vote job, which does know its own login, is
    stricter still.

    Dismissed reviews are ignored: a dismissal has already released the pull
    request, so it is not what is holding it.
    """
    rejections = [
        r for r in reviews or () if r.get("state") == "CHANGES_REQUESTED"
    ]
    # ANY human rejection parks it, at any age and whatever a bot also said.
    # Looking only at the bot ones and ignoring the rest was the first version of
    # this, and its own test caught it: a human rejection sitting beside a stale
    # bot one read as "stale", which would have re-reviewed over a maintainer.
    if any((r.get("user") or {}).get("type") != "Bot" for r in rejections):
        return False
    if not rejections:
        return False
    return all(r.get("commit_id") != sha for r in rejections)


def pr_reviews(repo, pr_number):
    """Every review on a pull request, paginated.

    `--paginate` is load-bearing, and so is parsing its output as a STREAM. The
    endpoint pages at 100 under `gh --paginate`, and a pull request that collects
    that many reviews would otherwise push the App's own past votes off the first
    page — silently defeating the guard on exactly the long-lived pull requests it
    matters most on. Past that boundary `gh` emits one document per page, which is
    why this goes through `gh_json_paginated` rather than `gh_json`.
    """
    return gh_json_paginated(
        "api", f"repos/{repo}/pulls/{pr_number}/reviews", "--paginate",
        "--jq", "[.[] | {commit_id, state, user: {login: .user.login, type: .user.type}}]",
    )


def pr_comments(repo, pr_number):
    """Every issue comment on a pull request, paginated. Same paging reason."""
    return gh_json_paginated(
        "api", f"repos/{repo}/issues/{pr_number}/comments", "--paginate",
        "--jq", "[.[] | {body, user: {login: .user.login}}]",
    )


def fail(message):
    print(f"::error title=Agent PR review::{message}", file=sys.stderr)
    sys.exit(1)
