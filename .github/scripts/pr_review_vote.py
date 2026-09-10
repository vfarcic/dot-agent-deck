#!/usr/bin/env python3
"""Cast the review the agent asked for, after re-validating everything itself.

This is the only place the App token exists, and it holds it precisely because
the agent must not. It re-checks the head SHA and the required contexts rather
than trusting the selection job, so a pull request that moved while the agent was
thinking is never voted on.

Failure policy: a MISSING or MALFORMED verdict fails the job. An agent that
produced nothing must be visible, not silently treated as "no opinion" — that is
the difference between a reviewer that is off and a reviewer that looks fine.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pr_review_common import (  # noqa: E402
    checks_green,
    fail,
    gh,
    gh_json,
    latest_verdict,
)


def main():
    repo = os.environ["REPO"]
    pr_number = os.environ["PR_NUMBER"]
    expected_sha = os.environ["EXPECTED_SHA"]
    vote_allowed = os.environ.get("VOTE_ALLOWED", "").lower() == "true"

    if not vote_allowed:
        print(f"#{pr_number}: touches a path that requires a human approval - no vote cast.")
        return

    # 1. The pull request must not have moved since the agent read it.
    current = gh_json(
        "pr", "view", pr_number, "--repo", repo, "--json", "headRefOid,isDraft,reviewDecision"
    )
    if current["headRefOid"] != expected_sha:
        print(
            f"#{pr_number}: head moved {expected_sha[:8]} -> {current['headRefOid'][:8]} "
            "during review; not voting. The next sweep will re-review."
        )
        return
    if current["isDraft"]:
        print(f"#{pr_number}: converted to draft during review; not voting.")
        return
    if current.get("reviewDecision") == "CHANGES_REQUESTED":
        print(f"#{pr_number}: a reviewer requested changes during review; not voting.")
        return

    # 2. The gates must still be green, checked here and not taken on trust.
    green, why = checks_green(repo, expected_sha)
    if not green:
        print(f"#{pr_number}: not voting - {why}")
        return

    # 3. There must be a verdict, and it must be for this exact SHA.
    try:
        verdict = latest_verdict(repo, pr_number)
    except ValueError as exc:
        fail(f"#{pr_number}: verdict comment is malformed: {exc}")
        return
    if verdict is None:
        fail(
            f"#{pr_number}: the reviewer produced no verdict comment. The agent step "
            "reported success but wrote nothing - treat this as a broken reviewer, "
            "not as an absent opinion."
        )
        return
    if verdict.get("head_sha") != expected_sha:
        fail(
            f"#{pr_number}: verdict is for {verdict['head_sha'][:8]} but head is "
            f"{expected_sha[:8]}; refusing to apply a stale verdict."
        )
        return
    if str(verdict.get("pr")) != str(pr_number):
        fail(f"#{pr_number}: verdict names pr {verdict.get('pr')}; refusing.")
        return

    # 4. Cast it.
    reasons = "\n".join(f"- {r}" for r in verdict.get("reasons", [])) or "- (none given)"
    decision = verdict["verdict"]
    if decision == "INSUFFICIENT":
        print(f"#{pr_number}: verdict INSUFFICIENT - no vote cast, human review needed.")
        return

    flag = "--approve" if decision == "APPROVE" else "--request-changes"
    body = (
        f"Automated review ({decision}) for `{expected_sha[:8]}`.\n\n{reasons}\n\n"
        "This vote was cast by the review App on the verdict linked above. "
        "It is a second opinion, not a substitute for one: any maintainer can "
        "still request changes, and unresolved review threads still block the merge."
    )
    gh("pr", "review", pr_number, "--repo", repo, flag, "--body", body)
    print(f"#{pr_number}: cast {decision} for {expected_sha[:8]}")


if __name__ == "__main__":
    main()
