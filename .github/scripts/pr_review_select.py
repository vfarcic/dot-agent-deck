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
    gh_json,
    latest_verdict,
    unresolved_threads,
)

# Authors whose pull requests are only eligible when they carry a specific label.
#
# Renovate labels its own pull requests `manual-review` at open time when they are
# NOT in an automerge group — i.e. exactly when a human has to look. (Verified: the
# label comes from renovate[bot], not from the adaptive labeler, which explicitly
# blocks it.) Reviewing the rest would spend tokens on pull requests that merge
# without anyone reading them: measured over 40 merged Renovate PRs, 27 carried no
# such label and merged at a 14h mean.
#
# An elapsed-time gate was considered and rejected on measurement: only 2 of those
# 40 ever reached 48h, and the labelled set merged no slower than the unlabelled
# one (15.6h vs 14.3h mean), so time does not select the set that needs review.
AUTHOR_REQUIRED_LABEL = {
    "app/renovate": "manual-review",
}


def env(name, default=""):
    return (os.environ.get(name) or default).strip()


def touches_denied(repo, number):
    files = gh_json(
        "api", f"repos/{repo}/pulls/{number}/files", "--paginate",
        "--jq", "[.[].filename]",
    ) or []
    return [f for f in files if any(f.startswith(p) for p in DENY_PATHS)]


def main():
    repo = env("REPO")
    only_pr = env("ONLY_PR")
    review_authors = set(env("REVIEW_AUTHORS").split())
    vote_authors = set(env("VOTE_AUTHORS").split())
    try:
        max_prs = max(1, int(env("MAX_PRS", "5")))
    except ValueError:
        max_prs = 5

    # Live requires BOTH the repo variable and a dispatch that did not ask for a
    # dry run. A scheduled tick has no dry_run input, so enabling the schedule
    # never by itself enables voting.
    live = env("REPO_LIVE") == "true" and env("DISPATCH_DRY_RUN") != "true"

    owner = repo.split("/")[0]
    prs = gh_json(
        "pr", "list", "--repo", repo, "--state", "open", "--limit", "100",
        "--json", "number,author,isDraft,headRefOid,headRepositoryOwner,reviewDecision,labels",
    ) or []

    # Maintainer pull requests first, so a burst of bot pull requests can never
    # crowd them out of the max_prs cap.
    prs.sort(key=lambda p: p["author"]["login"] in AUTHOR_REQUIRED_LABEL)

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

        required_label = AUTHOR_REQUIRED_LABEL.get(author)
        if required_label:
            names = [label["name"] for label in pr.get("labels", [])]
            if required_label not in names:
                skipped.append((number, f"{author} pull request without {required_label!r}"))
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

        # Idempotence: a current verdict means this sweep has nothing to add.
        existing = latest_verdict(repo, number)
        if existing and existing.get("head_sha") == sha:
            skipped.append((number, f"already has a verdict for {sha[:8]}"))
            continue

        denied = touches_denied(repo, number)
        items.append({
            "number": number,
            "sha": sha,
            "vote_allowed": author in vote_authors and not denied,
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
