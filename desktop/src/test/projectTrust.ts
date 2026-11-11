import { vi } from "vitest";

import type { ProjectTrust, ProjectTrustTarget } from "../lib/projectTrust";

/**
 * PRD #1885 — a test runtime's answer to the review a launch asks for first:
 * every orchestration approved, so a test about something else starts as it
 * did before the review existed. A test about the review itself passes its
 * own `projectTrustState` / `approveProjectTrust`.
 */
export function approvedProjectTrust() {
  const approved = async (target: ProjectTrustTarget): Promise<ProjectTrust> => ({
    kind: "review",
    project: target.path,
    orchestration: target.orchestration,
    state: "approved",
    roles: [],
    removedRoles: [],
    digest: "approved",
  });
  return {
    projectTrustState: vi.fn(approved),
    approveProjectTrust: vi.fn(approved),
  };
}
