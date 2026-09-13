#!/usr/bin/env python3
"""Cast the review the agent asked for, after re-validating everything itself.

This is the only place the App token exists, and it holds it precisely because
the agent must not. It re-checks the head SHA and the required contexts rather
than trusting the selection job, so a pull request that moved while the agent was
thinking is never voted on.

Failure policy: a MISSING or MALFORMED verdict fails the job. An agent that
produced nothing must be visible, not silently treated as "no opinion" — that is
the difference between a reviewer that is off and a reviewer that looks fine.

Issue #998 — sensitive paths. A pull request touching DENY_PATHS used to get no
vote at all, which left it waiting on a second maintainer: the exact situation
this reviewer exists to avoid, pushing the author back toward an admin bypass.
It now depends on whether a human will be the one merging:

  * auto-merge NOT armed -> vote, with the reason stated loudly in the review
    body and a label so the risk is visible from the pull request list.
  * auto-merge ARMED -> no vote, and say so on the pull request. A marker is
    worthless when nobody opens the page, and arming auto-merge is a
    declaration of "merge this without me looking" — on a sensitive path that
    is exactly when this reviewer should decline to be the only reader.

Either way the job now SAYS what it did. Before #998 the reason lived in a
`print()` that went to the workflow log, so a green verdict with no approval
looked like a bug and could only be explained by reading the run.

Issue #1050 - idempotence, which lives HERE and not in the selector. The vote
pass is selected on "has a current verdict", deliberately: that is what keeps a
verdict from an earlier run votable and a transiently-failed vote retryable. The
missing half was a bound, so the daily sweep re-cast the same approval on an
unchanged head twice a day. Every outcome below is now keyed on the head SHA -
the job asks whether it has already acted on THIS head before acting again. It
belongs in this job because the App's own identity is only known here, from the
token step's `app-slug`; the selector holds no App credential and cannot ask who
it is. Cost of that placement is one cheap `gh`-only matrix leg per already-voted
pull request, which is also what preserves the retry: a vote that failed left no
review at that SHA, so the next sweep sees nothing and casts it.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pr_review_common import (  # noqa: E402
    DENY_PRECEDENCE,
    already_noticed_at,
    already_reviewed_at,
    checks_green,
    fail,
    gh,
    gh_json,
    gh_ok,
    latest_verdict,
    pr_comments,
    pr_reviews,
)

ATTENTION_LABEL = "needs-human-eye"

# Why each deny-listed path needs a human, in the words the merger needs to read.
#
# Keyed by prefix, and the PRECEDENCE is DENY_PRECEDENCE in pr_review_common —
# not this dict's insertion order. Ordering lived here until #1000 review, which
# is what let it disagree with the selector's truncation of `denied_paths`; the
# selector now sorts by the same tuple, so one edit moves both. Adding a prefix
# here without adding it there leaves it unreachable, which
# `the_deny_reasons_and_the_precedence_cover_each_other` fails on.
DENY_REASONS = {
    "src/daemon_protocol.rs": (
        "This changes the TUI↔daemon protocol. **CLAUDE.md rule 12 requires a manual "
        "cross-version test** — a previous-release daemon started *with agents under it*, "
        "then this branch's TUI against it, confirming a delegate still routes and hooks "
        "still arrive. `tests/daemon_protocol.rs` cannot cover that: its 28 tests all "
        "compile from one source tree, so they prove the wire shape is self-consistent, "
        "not that an old and a new build interoperate. Nothing I can read tells me whether "
        "that test was run."
    ),
    ".github/workflows/pr-review": (
        "This edits **my own workflow**. I am not a trustworthy reviewer of changes to the "
        "thing that decides what I may approve."
    ),
    ".github/scripts/pr_review_": (
        "This edits **my own selection and voting logic**, including the rules that decide "
        "which pull requests I may approve."
    ),
    "CLAUDE.md": (
        "This edits the **rules I review against**. Approving it would let a pull request "
        "change its own criteria."
    ),
    "scripts/apply-branch-protection.sh": (
        "This edits the script that **reconstructs the branch-protection ruleset**, and "
        "`apply` is a full PUT that deletes every rule it does not emit."
    ),
    "MAINTAINERS.md": "This edits the **record of who the maintainers are**.",
    "greptile.json": "This edits the **other reviewer's configuration**.",
    ".github/": (
        "This edits **CI or repository automation**, which is how permissions and gates get "
        "widened."
    ),
}


def deny_reason(paths):
    """The most specific explanation for why these paths need a human.

    Precedence comes from DENY_PRECEDENCE, which the selector also sorts by — so
    the reason cannot depend on which paths survived its `denied_paths` truncation.
    """
    for prefix in DENY_PRECEDENCE:
        if any(p.startswith(prefix) for p in paths):
            return DENY_REASONS[prefix]
    return "This touches a path that requires a human approval."


def auto_merge_armed(repo, pr_number):
    """Read the auto-merge state fresh, at the moment it is about to be acted on.

    Deliberately re-read rather than reused from the step-1 snapshot: several
    network round-trips (the check sweep, the verdict fetch) sit between the two
    points, and arming auto-merge in that window would turn an approval into an
    unattended merge on a sensitive path. This NARROWS the window to one call; it
    does not close it. Nothing here can — GitHub offers no way to approve and
    assert "and auto-merge was not armed" atomically, so a caller arming it
    microseconds after this returns still wins. One call's width is the floor.
    """
    current = gh_json(
        "pr", "view", pr_number, "--repo", repo, "--json", "autoMergeRequest"
    )
    return (current or {}).get("autoMergeRequest") is not None


def add_label(repo, pr_number):
    """Best-effort. Needs `issues: write` on the App; a failure must not lose the vote."""
    if not gh_ok("pr", "edit", pr_number, "--repo", repo, "--add-label", ATTENTION_LABEL):
        print(
            f"#{pr_number}: could not apply the {ATTENTION_LABEL!r} label. The App needs "
            "`issues: write`, and the label must exist. The review body still carries the "
            "reason, so this is a lost signal rather than a lost gate."
        )
        return False
    return True


def clear_label(repo, pr_number):
    """Drop the attention marker when the diff is no longer sensitive.

    The label is applied per-review, but it outlives the diff that earned it: a
    later push can remove every protected path, and the ordinary path would then
    vote normally while the pull request still advertises itself as needing a
    human eye. `--remove-label` on a pull request that does not carry it is a
    no-op that exits 0, so this needs no "is it there" probe.
    """
    gh_ok("pr", "edit", pr_number, "--repo", repo, "--remove-label", ATTENTION_LABEL)


def attention_body(decision, reason, touched, sha, reasons):
    """The review body for a sensitive pull request the App is still voting on.

    Heading and closing both follow `decision`. Submitting `--request-changes`
    under an "Approved" heading told the author the opposite of what was decided,
    precisely when defects had been found, so the two are derived here together
    rather than written once and reused across both verdicts.
    """
    if decision == "APPROVE":
        heading = "## ⚠️ Approved, but read this before merging"
        closing = (
            "I reviewed the diff and found it sound, so this approval satisfies the "
            "required review and you are not waiting on a second maintainer. It is "
            "**not** a statement that the obligation above has been met — I cannot see "
            "whether it has. You are the human in this loop."
        )
    else:
        heading = "## ⚠️ Changes requested — and read this before merging"
        closing = (
            "I reviewed the diff and found the defects above, so this is a rejection "
            "and **not** an approval: it does not satisfy the required review. The "
            "obligation above stands on top of them, and I cannot see whether it has "
            "been met. You are the human in this loop."
        )
    return (
        f"{heading}\n\n{reason}\n\n"
        f"**Touches:** {touched}\n\n"
        f"---\n\nAutomated review (`{decision}`) for `{sha[:8]}`.\n\n{reasons}\n\n"
        f"{closing}"
    )


def comment(repo, pr_number, body):
    gh_ok("pr", "comment", pr_number, "--repo", repo, "--body", body)


def main():
    repo = os.environ["REPO"]
    pr_number = os.environ["PR_NUMBER"]
    expected_sha = os.environ["EXPECTED_SHA"]
    # The App's own login, as `<app-slug>[bot]`. Supplied by the token step rather
    # than hardcoded, so renaming the App cannot silently un-key the guards below.
    app_login = (os.environ.get("APP_LOGIN") or "").strip()
    vote_allowed = os.environ.get("VOTE_ALLOWED", "").lower() == "true"
    try:
        denied_paths = json.loads(os.environ.get("DENIED_PATHS") or "[]")
    except json.JSONDecodeError:
        denied_paths = []

    # 1. The pull request must not have moved since the agent read it.
    current = gh_json(
        "pr",
        "view",
        pr_number,
        "--repo",
        repo,
        "--json",
        "headRefOid,isDraft,reviewDecision",
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

    if not app_login:
        # Fail OPEN and say so. Refusing to vote here would block every merge this
        # reviewer exists to unblock; voting twice is merely noise. See
        # already_reviewed_at() for why that is the right way round.
        print(
            f"::warning title=Agent PR review::#{pr_number}: APP_LOGIN is empty, so I "
            "cannot tell my own past votes apart from anyone else's. Voting without the "
            "per-SHA guard - this may duplicate an approval (issue #1050). Check the "
            "`app-slug` output on the token step."
        )

    # 2. I must not already have voted on this exact head (issue #1050).
    #
    # Before the gates, because it is the cheapest way out and because a head I
    # have already voted on needs no re-verification: the vote that stands was
    # cast against these same contexts at this same SHA.
    if already_reviewed_at(pr_reviews(repo, pr_number), expected_sha, app_login):
        print(
            f"#{pr_number}: already reviewed {expected_sha[:8]}; not voting again. "
            "A push moves the head and makes it votable once more."
        )
        return

    # 3. The gates must still be green, checked here and not taken on trust.
    green, why = checks_green(repo, expected_sha)
    if not green:
        print(f"#{pr_number}: not voting - {why}")
        return

    # 4. There must be a verdict, and it must be for this exact SHA.
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

    decision = verdict["verdict"]
    reasons = "\n".join(f"- {r}" for r in verdict.get("reasons", [])) or "- (none given)"

    if decision == "INSUFFICIENT":
        # Say it once per head. The verdict is fixed for this SHA, so a re-post
        # carries no new information and only teaches the reader to scroll past it.
        if already_noticed_at(pr_comments(repo, pr_number), expected_sha, app_login):
            print(
                f"#{pr_number}: verdict INSUFFICIENT and already said so for "
                f"{expected_sha[:8]}; not repeating it."
            )
            return
        comment(
            repo,
            pr_number,
            f"**No vote cast** for `{expected_sha[:8]}` — my verdict was `INSUFFICIENT`, "
            "meaning I could not review this confidently. This needs a human review; it is "
            "not an approval and not a rejection.",
        )
        print(f"#{pr_number}: verdict INSUFFICIENT - no vote cast, human review needed.")
        return

    # 5. Sensitive paths: whether a human will be the one merging decides this.
    if not vote_allowed:
        reason = deny_reason(denied_paths)
        touched = ", ".join(f"`{p}`" for p in denied_paths[:5]) or "a protected path"
        # Read this last, not from the step-1 snapshot: see auto_merge_armed().
        if auto_merge_armed(repo, pr_number):
            # Only suppress the repeat while auto-merge is STILL armed, which is
            # what keeps the notice's own instruction honest: disarming drops out
            # of this branch entirely and the job votes. Reached only when armed,
            # so the guard can never strand a pull request the reader has acted on.
            if already_noticed_at(pr_comments(repo, pr_number), expected_sha, app_login):
                print(
                    f"#{pr_number}: deny-listed, auto-merge still armed, and already said "
                    f"so for {expected_sha[:8]}; not repeating it."
                )
                return
            comment(
                repo,
                pr_number,
                f"**No vote cast** for `{expected_sha[:8]}`, even though my verdict was "
                f"`{decision}`.\n\n{reason}\n\nTouches: {touched}\n\n"
                "**Auto-merge is armed on this pull request.** Approving it would merge it "
                "with no human ever opening the page, and a warning nobody reads is not a "
                "safeguard. Disarm auto-merge and re-run the reviewer if you want my "
                "approval on the record; merge it yourself if you would rather not.",
            )
            print(
                f"#{pr_number}: deny-listed AND auto-merge armed - no vote. "
                f"verdict was {decision}."
            )
            return

        flag = "--approve" if decision == "APPROVE" else "--request-changes"
        labelled = add_label(repo, pr_number)
        body = attention_body(decision, reason, touched, expected_sha, reasons)
        gh("pr", "review", pr_number, "--repo", repo, flag, "--body", body)
        print(
            f"#{pr_number}: cast {decision} for {expected_sha[:8]} with an attention marker "
            f"(label {'applied' if labelled else 'NOT applied'}); touches {denied_paths[:3]}"
        )
        return

    # 6. The ordinary case. Nothing protected is touched at this SHA, so an
    # attention marker from an earlier one is now a lie the pull request list tells.
    clear_label(repo, pr_number)
    body = (
        f"Automated review (`{decision}`) for `{expected_sha[:8]}`.\n\n{reasons}\n\n"
        "This vote was cast by the review App on the verdict linked above. "
        "It is a second opinion, not a substitute for one: any maintainer can "
        "still request changes, and unresolved review threads still block the merge."
    )
    flag = "--approve" if decision == "APPROVE" else "--request-changes"
    gh("pr", "review", pr_number, "--repo", repo, flag, "--body", body)
    print(f"#{pr_number}: cast {decision} for {expected_sha[:8]}")


if __name__ == "__main__":
    main()
