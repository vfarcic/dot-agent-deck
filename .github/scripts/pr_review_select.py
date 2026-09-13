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
    checks_green,
    deny_sort_key,
    gh_json,
    gh_json_paginated,
    latest_verdict,
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
            skipped.append((number, "changes requested by a reviewer"))
            continue

        green, why = checks_green(repo, sha)
        if not green:
            skipped.append((number, why))
            continue

        open_threads = unresolved_threads(repo, number)
        if open_threads:
            skipped.append((number, f"{open_threads} unresolved review thread(s)"))
            continue

        existing = latest_verdict(repo, number)
        has_current_verdict = bool(existing and existing.get("head_sha") == sha)

        if mode == "review" and has_current_verdict:
            # Idempotence: nothing to add for a head that already has a verdict.
            skipped.append((number, f"already has a verdict for {sha[:8]}"))
            continue
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
