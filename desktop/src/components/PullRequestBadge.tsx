import { CircleAlert, CircleCheck, CircleDashed, Eye, GitMerge, GitPullRequest, GitPullRequestClosed, GitPullRequestDraft } from "lucide-react";
import type { AgentPullRequest } from "../types";
import type { PullRequestReview, PullRequestState } from "../lib/bridge";
import { pullRequestLabel } from "../lib/pullRequest";

/*
  PRD #1401 — the pull request badge, in a module of its own because two
  screens draw it: the agent tile and screen (`AgentTile`) and the overview
  dashboard's rows (`AgentOverview`), which `AgentTile` itself imports from.
*/

const PR_STATE_ICON: Record<PullRequestState, typeof GitPullRequest> = {
  open: GitPullRequest,
  draft: GitPullRequestDraft,
  merged: GitMerge,
  closed: GitPullRequestClosed,
  unknown: GitPullRequest,
};

const PR_REVIEW_ICON: Record<PullRequestReview, typeof GitPullRequest> = {
  approved: CircleCheck,
  changes_requested: CircleAlert,
  review_required: Eye,
  unknown: CircleDashed,
};

/**
 * PRD #1401 — the badge: the PR's number between an icon for its state and
 * one for its review decision, and nothing else (decision 1 of 2026-10-05).
 * Its words are in the accessible name and the tooltip — "Pull request #1234:
 * open, review required" — so the icons carry no meaning only colour shows.
 *
 * A button when it can open the in-app browser (the shell offers one and the
 * PR has a github.com address), plain text otherwise.
 */
export function PullRequestBadge({ pullRequest, testId, onOpen }: { pullRequest: AgentPullRequest; testId?: string; onOpen?: () => void }) {
  const label = pullRequestLabel(pullRequest);
  const StateIcon = PR_STATE_ICON[pullRequest.state];
  const ReviewIcon = pullRequest.review ? PR_REVIEW_ICON[pullRequest.review] : undefined;
  const className = `pr-badge pr-state-${pullRequest.state}${pullRequest.review ? ` pr-review-${pullRequest.review}` : ""}`;
  const content = (
    <>
      <StateIcon className="pr-badge-state" size={11} aria-hidden="true" />
      <span className="pr-badge-number">#{pullRequest.number}</span>
      {ReviewIcon && <ReviewIcon className="pr-badge-review" size={11} aria-hidden="true" />}
    </>
  );
  const data = { "data-testid": testId, "data-pr-state": pullRequest.state, "data-pr-review": pullRequest.review };
  return onOpen ? (
    <button
      type="button"
      className={className}
      title={`${label} — open it in the app`}
      aria-label={label}
      onMouseDown={(event) => event.stopPropagation()}
      onClick={onOpen}
      {...data}
    >{content}</button>
  ) : (
    <span className={className} title={label} role="img" aria-label={label} {...data}>{content}</span>
  );
}
