#!/usr/bin/env python3
"""Select which pull requests the agent reviewer should look at.

Runs before any agent does, on plain `gh` calls only. When nothing is eligible it
emits an empty set and the review job is skipped entirely, so a quiet sweep costs
zero agent tokens — which is the point of doing selection here rather than asking
the agent to find its own work.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pr_review_common import (  # noqa: E402
    DENY_PATHS,
    bot_rejection_is_stale,
    check_status,
    deny_sort_key,
    gh_json,
    gh_json_paginated,
    focus_pass_requested,
    latest_verdict,
    pr_reviews,
    unresolved_threads,
)

# Authors ranked BEHIND the maintainers inside the `max_prs` cap.
#
# This is an ordering, not a gate. It used to be one — Renovate pull requests were
# eligible only when they carried `manual-review` — and that gate is gone (issue
# #1050 review). What it was reaching for was sound: do not spend tokens reviewing
# pull requests that merge without anyone reading them, measured at the time as 27
# of 40 merged Renovate PRs carrying no such label, at a 14h mean.
#
# The label was a PROXY for "a human has to look at this", and the proxy had a
# false-negative class nobody had measured. It is applied by explicit `labels:`
# arrays on individual `renovate.json` packageRules, NOT — as the comment here
# used to claim — by a pull request "not being in an automerge group". npm updates
# outside `site/**` are in no automerge group AND carry no such label, so they were
# held for a human and skipped by the reviewer at the same time: #1018, #1037 and
# #1039 sat for days with green CI and no review, while #1039's sibling #1038 —
# the identical React 19.3.0 bump against `site/package-lock.json` — automerged
# itself the same day.
#
# There is no direct signal to replace the proxy with. Renovate merges through its
# own API call rather than GitHub auto-merge, so `autoMergeRequest` is null on
# every Renovate pull request whether it will automerge or not (measured across
# #1038, #1036, #1017, #1007, #1002 and #1001: zero `auto_merge_enabled` events on
# any of them). So the choice was an inaccurate proxy or no proxy, and no proxy
# wins: the cost of reviewing a pull request that would have merged anyway is one
# model call on a lockfile diff, and the cost of skipping one that would not is
# what those three pull requests did for days.
#
# The ordering survives because dropping the gate makes it MORE load-bearing, not
# less: bot pull requests now reach selection in bulk, and without this they could
# crowd maintainer pull requests out of `max_prs`.
DEPRIORITISED_AUTHORS = {"app/renovate"}


def env(name, default=""):
    return (os.environ.get(name) or default).strip()


def touches_denied(repo, number):
    """The protected paths this pull request touches, most-specific-reason first.

    Sorted rather than left in the API's order, because `denied_paths` is truncated
    before it reaches the vote job and that job explains the pull request by the
    first match. The API returns paths alphabetically, which put `.github/` first
    and `src/daemon_protocol.rs` last — the exact inverse of how severe they are.
    """
    # Streamed, not `gh_json`: past 100 files `gh --paginate` emits one JSON
    # document per page and a single `json.loads` raises. This is the call site
    # closest to that boundary — PR #1035 carries 70 files — and it raises out of
    # the selection loop, so it would take the WHOLE sweep down rather than one
    # pull request. Issue #1050 review.
    files = gh_json_paginated(
        "api", f"repos/{repo}/pulls/{number}/files", "--paginate",
        "--jq", "[.[].filename]",
    )
    denied = [f for f in files if any(f.startswith(p) for p in DENY_PATHS)]
    return sorted(denied, key=deny_sort_key)


def main():
    repo = env("REPO")
    only_pr = env("ONLY_PR")
    # "review" selects pull requests that NEED a verdict; "vote" selects those
    # that HAVE a current one and may be voted on. Two passes, because a verdict
    # produced by this run must still be votable, and a verdict produced by an
    # earlier run must not be orphaned just because it is no longer new.
    mode = env("SELECT_MODE", "review")
    # Issue #1266: a FOCUSED follow-up pass re-reviews a head that already has a
    # verdict, covering only what earlier verdicts did not. Idempotence below
    # would otherwise skip it, which is right for every other caller: the sweep
    # must not pay for a head twice.
    #
    # ONLY_PR is required, and that is the guard rather than a convenience. The
    # flag reaches this script only from a manual `workflow_dispatch`, but an
    # input is a string someone can also set on a sweep, and a focused SWEEP
    # would re-review every eligible head on every run — unbounded spend from one
    # true-ish value. Requiring a single named pull request bounds it to the one
    # the operator asked for.
    focus_unreviewed = focus_pass_requested(env("FOCUS_UNREVIEWED", ""), only_pr)
    review_authors = set(env("REVIEW_AUTHORS").split())
    vote_authors = set(env("VOTE_AUTHORS").split())
    try:
        max_prs = max(1, int(env("MAX_PRS", "5")))
    except ValueError:
        max_prs = 5

    # Live requires ALL of: the repo variable, an event that is allowed to vote,
    # and a dispatch that did not ask for a dry run.
    #
    # The event check is not redundant. A pull_request event carries no dry_run
    # input, so without it live-ness would fall back to the repo variable alone —
    # meaning a temporary pull_request trigger added for testing could cast real
    # approvals. Voting events are named explicitly so that adding any new trigger
    # is inert until someone decides otherwise.
    # A pull_request run can never vote — that keeps a temporary test trigger,
    # or any trigger added later, inert until someone decides otherwise. The
    # schedule CAN vote, which makes PR_REVIEW_LIVE the single honest switch:
    # setting it authorises approvals, including unattended ones on the daily
    # sweep. That is deliberate — one clear switch beats two overlapping guards,
    # where "can it vote?" depended on both the variable and which trigger
    # fired, and someone could reasonably believe voting was off when it wasn't.
    voting_events = {"schedule", "workflow_dispatch"}
    live = (
        env("REPO_LIVE") == "true"
        and env("EVENT_NAME") in voting_events
        and env("DISPATCH_DRY_RUN") != "true"
    )

    owner = repo.split("/")[0]
    prs = gh_json(
        "pr", "list", "--repo", repo, "--state", "open", "--limit", "100",
        "--json", "number,author,isDraft,headRefOid,headRepositoryOwner,reviewDecision",
    ) or []

    # Maintainer pull requests first, so a burst of bot pull requests can never
    # crowd them out of the max_prs cap.
    prs.sort(key=lambda p: p["author"]["login"] in DEPRIORITISED_AUTHORS)

    items, skipped = [], []
    # Advisory reds, per selected pull request. Kept out of `items` on purpose:
    # that list becomes the review job's matrix payload, and this is for the log.
    advisories = {}
    for pr in prs:
        number, sha = pr["number"], pr["headRefOid"]
        author = pr["author"]["login"]

        if only_pr and str(number) != only_pr:
            continue
        if pr["isDraft"]:
            skipped.append((number, "draft"))
            continue
        if author not in review_authors:
            skipped.append((number, f"author {author} not in REVIEW_AUTHORS"))
            continue
        if (pr.get("headRepositoryOwner") or {}).get("login") != owner:
            skipped.append((number, "fork"))
            continue

        if pr.get("reviewDecision") == "CHANGES_REQUESTED":
            # ... unless the only thing holding it is OUR OWN rejection, on a head
            # that has since moved. `dismiss_stale_reviews_on_push` dismisses
            # approvals and not changes-requested, so without this a pull request
            # the reviewer rejected was excluded from the reviewer FOREVER: the
            # author pushes a fix and nothing ever looks again (#1019 needed a
            # manual dismissal to escape). A human's changes-requested still
            # parks it — that is someone else's homework.
            if not bot_rejection_is_stale(pr_reviews(repo, number), sha):
                skipped.append((number, "changes requested by a reviewer"))
                continue
            print(f"note #{number}: re-reviewing; my own rejection predates {sha[:8]}")

        # Issue #1086: the gate is the contexts branch protection REQUIRES, read
        # from the live ruleset. It used to be "no check on the head has failed",
        # which made every advisory job a veto — and because this reviewer's
        # approval is what satisfies the required-review rule, an advisory red
        # blocked the only thing that could unblock the merge. PR #1076 lost its
        # approval to a `devbox` job that died on a third-party CDN's 504 with all
        # five required contexts green.
        green, why, advisory = check_status(repo, sha)
        if not green:
            skipped.append((number, why))
            continue
        if advisory:
            advisories[number] = advisory

        open_threads = unresolved_threads(repo, number)
        if open_threads:
            skipped.append((number, f"{open_threads} unresolved review thread(s)"))
            continue

        existing = latest_verdict(repo, number)
        has_current_verdict = bool(existing and existing.get("head_sha") == sha)

        if mode == "review" and has_current_verdict and not focus_unreviewed:
            # Idempotence: nothing to add for a head that already has a verdict.
            skipped.append((number, f"already has a verdict for {sha[:8]}"))
            continue
        if mode == "review" and has_current_verdict and focus_unreviewed:
            # Said out loud because the run costs credits and the operator asked
            # for it: a focused pass is the ONE case where paying twice for a
            # head is the point. What makes it worth paying is that the second
            # verdict covers what the first one declined to, which the agent
            # reads out of the earlier verdict's `covered_paths`.
            print(f"note #{number}: focused re-review of {sha[:8]}; it already has a verdict")
        if mode == "vote" and not has_current_verdict:
            skipped.append((number, f"no current verdict for {sha[:8]}"))
            continue

        denied = touches_denied(repo, number)
        vote_allowed = author in vote_authors and not denied
        # A deny-listed pull request is still SELECTED in vote mode (issue #998):
        # the vote job decides between approving it with a visible attention
        # marker and declining, based on whether auto-merge is armed. Only an
        # author outside VOTE_AUTHORS is dropped here, since nothing downstream
        # would act on it.
        if mode == "vote" and author not in vote_authors:
            skipped.append((number, f"author {author} not in VOTE_AUTHORS"))
            continue

        items.append({
            "number": number,
            "sha": sha,
            "vote_allowed": vote_allowed,
            # Truncated to keep the matrix payload small, and safe to truncate
            # ONLY because touches_denied sorted by deny_sort_key: the governing
            # path is now at index 0, so the vote job's explanation cannot depend
            # on what fell off the end. It still shortens the "Touches:" line on a
            # pull request with more than five protected paths, which is a display
            # limit rather than a lost reason.
            "denied_paths": denied[:5],
        })
        if len(items) >= max_prs:
            break

    for number, why in skipped:
        print(f"skip #{number}: {why}")
    for item in items:
        note = "" if item["vote_allowed"] else f"  (verdict only: {item['denied_paths'] or 'author not in VOTE_AUTHORS'})"
        print(f"review #{item['number']} @ {item['sha'][:8]}{note}")
        # Said out loud rather than swallowed: "selected anyway" is a different
        # statement from "nothing was red", and the reader should not have to
        # open the pull request to tell them apart.
        if item["number"] in advisories:
            failures = ", ".join(advisories[item["number"]])
            print(f"note #{item['number']}: advisory (not required, not a gate): {failures}")
    print(f"\nselected {len(items)}, live={live}")

    if only_pr and not items:
        # An explicit request that selects nothing is a mistake worth surfacing,
        # unlike an empty sweep which is the normal quiet case.
        print(f"::warning::#{only_pr} was requested but is not eligible; see skip reasons above")

    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as out:
        out.write(f"items={json.dumps(items, separators=(',', ':'))}\n")
        out.write(f"count={len(items)}\n")
        out.write(f"live={'true' if live else 'false'}\n")


if __name__ == "__main__":
    main()
