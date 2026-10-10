import type { AgentPullRequest } from "../types";
import type { PullRequestInfoDto, PullRequestReview, PullRequestState } from "./bridge";

/**
 * PRD #1401 — the agent's pull request, as the badge and the in-app browser
 * read it.
 *
 * The daemon resolves it with `gh` and sends it on the agent's record. This is
 * the projection's one check: the number must be a PR number, and the URL is
 * kept only when it is a github.com pull request page, because that URL is what
 * the badge opens. A record whose URL fails keeps its badge and opens nothing —
 * the number and the state are still true, and the in-app browser refuses the
 * same URL Rust-side anyway (`pr_browser::is_pull_request_url`).
 */
export function pullRequestFromDto(dto: PullRequestInfoDto | undefined): AgentPullRequest | undefined {
  if (!dto || !Number.isSafeInteger(dto.number) || dto.number <= 0) return undefined;
  const state: PullRequestState = KNOWN_STATES.includes(dto.state) ? dto.state : "unknown";
  const review: PullRequestReview | undefined = dto.review === undefined ? undefined : KNOWN_REVIEWS.includes(dto.review) ? dto.review : "unknown";
  return {
    number: dto.number,
    ...(typeof dto.url === "string" && isPullRequestUrl(dto.url) ? { url: dto.url } : {}),
    state,
    ...(review ? { review } : {}),
  };
}

const KNOWN_STATES: readonly PullRequestState[] = ["open", "draft", "merged", "closed", "unknown"];
const KNOWN_REVIEWS: readonly PullRequestReview[] = ["approved", "changes_requested", "review_required", "unknown"];

/** `https://github.com/<owner>/<repo>/pull/<number>` and nothing else — the same rule as `pr_browser::is_pull_request_url`. */
export function isPullRequestUrl(value: string): boolean {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "https:" || url.hostname.toLowerCase() !== "github.com" || url.port !== "" || url.username !== "" || url.password !== "") return false;
  const [owner, repo, pull, number] = url.pathname.split("/").slice(1);
  return Boolean(owner) && Boolean(repo) && pull === "pull" && /^[0-9]+$/.test(number ?? "");
}

/** The state in the badge's words. */
export const PULL_REQUEST_STATE_LABEL: Record<PullRequestState, string> = {
  open: "open",
  draft: "draft",
  merged: "merged",
  closed: "closed",
  unknown: "state unknown",
};

/** The review decision in the badge's words. */
export const PULL_REQUEST_REVIEW_LABEL: Record<PullRequestReview, string> = {
  approved: "approved",
  changes_requested: "changes requested",
  review_required: "review required",
  unknown: "review status unknown",
};

/** The badge's accessible name and tooltip: "Pull request #1234: open, review required". */
export function pullRequestLabel(pullRequest: AgentPullRequest): string {
  const parts = [PULL_REQUEST_STATE_LABEL[pullRequest.state]];
  if (pullRequest.review) parts.push(PULL_REQUEST_REVIEW_LABEL[pullRequest.review]);
  return `Pull request #${pullRequest.number}: ${parts.join(", ")}`;
}
