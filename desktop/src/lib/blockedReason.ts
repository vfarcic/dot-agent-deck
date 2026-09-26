import type { AgentBlocked } from "../types";
import { DISPLAY_LIMITS, displayText } from "./displayText";

/**
 * Issue #714: the fixed wording for each quota-block kind — the same words the
 * TUI card uses (`BlockedKind::label` in `src/quota_detect.rs`).
 */
const KIND_LABEL: Record<AgentBlocked["kind"], string> = {
  usage_limit: "Usage limit reached",
  credits_depleted: "Credits depleted (no reset)",
  unknown: "Provider limit reached",
};

/**
 * The reason line a blocked tile prints: the fixed label for the kind, then the
 * pane's own matched line. That line is agent-controlled text, so it goes
 * through `displayText` here even though the crate already scrubbed it — the
 * render seam checks rather than trusts.
 */
export function blockedReasonText(blocked: AgentBlocked | undefined): string {
  const label = KIND_LABEL[blocked?.kind ?? "unknown"];
  const detail = blocked?.detail ? displayText(blocked.detail, DISPLAY_LIMITS.message) : "";
  return detail ? `${label} — ${detail}` : label;
}
