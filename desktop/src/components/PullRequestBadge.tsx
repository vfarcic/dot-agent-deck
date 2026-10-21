import { CheckIcon, DotFillIcon, EyeIcon, FileDiffIcon, GitMergeIcon, GitPullRequestClosedIcon, GitPullRequestDraftIcon, GitPullRequestIcon, QuestionIcon, XIcon, type Icon } from "@primer/octicons-react";
import type { AgentPullRequest } from "../types";
import type { PullRequestChecksState, PullRequestReview, PullRequestState } from "../lib/bridge";
import { pullRequestLabel } from "../lib/pullRequest";

/*
  PRD #1401 — the pull request badge, in a module of its own because two
  screens draw it: the agent tile and screen (`AgentTile`) and the overview
  dashboard's rows (`AgentOverview`), which `AgentTile` itself imports from.

  Issue #1781 — its icons are GitHub's own (Octicons), in GitHub's colours, so
  the badge reads the way the pull request does on GitHub.
*/

const PR_STATE_ICON: Record<PullRequestState, Icon> = {
  open: GitPullRequestIcon,
  draft: GitPullRequestDraftIcon,
  merged: GitMergeIcon,
  closed: GitPullRequestClosedIcon,
  /* A question mark, as the TUI's `?`: a state the deck could not read is not
     an open pull request, and showing the open icon said it was. */
  unknown: QuestionIcon,
};

/* GitHub's pull request list: a green check, a red cross, an amber dot. */
const PR_CHECKS_ICON: Record<PullRequestChecksState, Icon> = {
  passing: CheckIcon,
  failing: XIcon,
  pending: DotFillIcon,
  unknown: QuestionIcon,
};

/* GitHub's reviewer icons: approved is a check and changes requested a file
   diff; a review still required is its eye. The badge draws them in a filled
   circle, so an approval never reads as the passing-CI check beside it. */
const PR_REVIEW_ICON: Record<PullRequestReview, Icon> = {
  approved: CheckIcon,
  changes_requested: FileDiffIcon,
  review_required: EyeIcon,
  unknown: QuestionIcon,
};

/**
 * PRD #1401 — the badge: an icon for the PR's state, its number, then its CI
 * result (issue #1781) and its review decision, and nothing else (decision 1
 * of 2026-10-05). Its words are in the accessible name and the tooltip —
 * "Pull request #1234: open, review required; CI: 24 passing" — so the icons
 * carry no meaning only colour shows.
 *
 * A button when it can open the in-app browser (the shell offers one and the
 * PR has a github.com address), plain text otherwise.
 */
export function PullRequestBadge({ pullRequest, testId, onOpen }: { pullRequest: AgentPullRequest; testId?: string; onOpen?: () => void }) {
  const label = pullRequestLabel(pullRequest);
  const StateIcon = PR_STATE_ICON[pullRequest.state];
  const ChecksIcon = pullRequest.checks ? PR_CHECKS_ICON[pullRequest.checks.state] : undefined;
  const ReviewIcon = pullRequest.review ? PR_REVIEW_ICON[pullRequest.review] : undefined;
  const className = `pr-badge pr-state-${pullRequest.state}${pullRequest.checks ? ` pr-checks-${pullRequest.checks.state}` : ""}${pullRequest.review ? ` pr-review-${pullRequest.review}` : ""}`;
  const content = (
    <>
      <StateIcon className="pr-badge-state" size={11} aria-hidden="true" />
      <span className="pr-badge-number">#{pullRequest.number}</span>
      {ChecksIcon && <ChecksIcon className="pr-badge-checks" size={11} aria-hidden="true" />}
      {ReviewIcon && (
        <span className="pr-badge-review" aria-hidden="true">
          <ReviewIcon size={8} aria-hidden="true" />
        </span>
      )}
    </>
  );
  const data = { "data-testid": testId, "data-pr-state": pullRequest.state, "data-pr-checks": pullRequest.checks?.state, "data-pr-review": pullRequest.review };
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
