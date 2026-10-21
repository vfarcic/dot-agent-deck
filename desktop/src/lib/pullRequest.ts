import type { AgentPullRequest, AgentPullRequestChecks } from "../types";
import type { PullRequestChecksDto, PullRequestChecksState, PullRequestInfoDto, PullRequestReview, PullRequestState } from "./bridge";

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
  const checks = checksFromDto(dto.checks);
  return {
    number: dto.number,
    ...(typeof dto.url === "string" && isPullRequestUrl(dto.url) ? { url: dto.url } : {}),
    state,
    ...(review ? { review } : {}),
    ...(checks ? { checks } : {}),
  };
}

/** Issue #1781 — the CI summary, with a result this build does not know read as unknown and a count that is not one read as none. */
function checksFromDto(dto: PullRequestChecksDto | undefined): AgentPullRequestChecks | undefined {
  if (!dto || typeof dto !== "object") return undefined;
  const count = (value: unknown) => (typeof value === "number" && Number.isSafeInteger(value) && value > 0 ? value : 0);
  return {
    state: KNOWN_CHECKS.includes(dto.state) ? dto.state : "unknown",
    passing: count(dto.passing),
    failing: count(dto.failing),
    pending: count(dto.pending),
    skipped: count(dto.skipped),
  };
}

const KNOWN_STATES: readonly PullRequestState[] = ["open", "draft", "merged", "closed", "unknown"];
const KNOWN_REVIEWS: readonly PullRequestReview[] = ["approved", "changes_requested", "review_required", "unknown"];
const KNOWN_CHECKS: readonly PullRequestChecksState[] = ["passing", "failing", "pending", "unknown"];

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

/** Issue #1781 — the CI result in the badge's words, when there are no counts to give. */
export const PULL_REQUEST_CHECKS_LABEL: Record<PullRequestChecksState, string> = {
  passing: "passing",
  failing: "failing",
  pending: "pending",
  unknown: "status unknown",
};

/** Issue #1781 — "CI: 1 failing, 24 passing": every non-empty bucket, the ones that need attention first. */
export function pullRequestChecksLabel(checks: AgentPullRequestChecks): string {
  if (checks.state === "unknown") return `CI: ${PULL_REQUEST_CHECKS_LABEL.unknown}`;
  const counts = ([["failing", checks.failing], ["pending", checks.pending], ["passing", checks.passing], ["skipped", checks.skipped]] as const)
    .filter(([, count]) => count > 0)
    .map(([word, count]) => `${count} ${word}`);
  return `CI: ${counts.length > 0 ? counts.join(", ") : PULL_REQUEST_CHECKS_LABEL[checks.state]}`;
}

/**
 * The badge's accessible name and tooltip: "Pull request #1234: open, review
 * required", and with checks "…; CI: 1 failing, 24 passing" (issue #1781). The
 * CI part goes last, after a semicolon, because its own counts are a list.
 */
export function pullRequestLabel(pullRequest: AgentPullRequest): string {
  const parts = [PULL_REQUEST_STATE_LABEL[pullRequest.state]];
  if (pullRequest.review) parts.push(PULL_REQUEST_REVIEW_LABEL[pullRequest.review]);
  const label = `Pull request #${pullRequest.number}: ${parts.join(", ")}`;
  return pullRequest.checks ? `${label}; ${pullRequestChecksLabel(pullRequest.checks)}` : label;
}
