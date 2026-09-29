import type { AgentBlocked } from "../types";
import { DISPLAY_LIMITS, displayText } from "./displayText";

/**
 * Issue #714: the fixed wording for each quota-block kind — the same words the
 * TUI card uses (`BlockedKind::label` in `src/quota_block.rs`).
 */
const KIND_LABEL: Record<AgentBlocked["kind"], string> = {
  usage_limit: "Usage limit reached",
  credits_depleted: "Credits depleted (no reset)",
  unknown: "Provider limit reached",
};

/**
 * A span the way the TUI card says it (`format_idle_elapsed` in
 * `src/state.rs`): seconds, minutes, whole hours, or `2h 10m`.
 */
export function formatSpan(ms: number): string {
  const plural = (n: number, unit: string) => `${n} ${unit}${n === 1 ? "" : "s"}`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return plural(seconds, "second");
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return plural(minutes, "minute");
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? plural(hours, "hour") : `${hours}h ${rest}m`;
}

/**
 * How far ahead a reset may lie and still be shown — the crate's
 * `QUOTA_RESET_MAX_FUTURE_MS` (`src/quota_block.rs`), past which the TUI card
 * shows no countdown either.
 */
const RESET_MAX_FUTURE_MS = 400 * 24 * 60 * 60 * 1000;

/**
 * The reason line a blocked tile prints: the fixed label for the kind, when the
 * provider said the limit resets (if that is still ahead of `nowMs`, and within
 * a plausible window of it), then the
 * agent's own error message. That message is agent-controlled text, so it goes
 * through `displayText` here even though the crate already scrubbed it — the
 * render seam checks rather than trusts.
 */
export function blockedReasonText(blocked: AgentBlocked | undefined, nowMs: number = Date.now()): string {
  const label = KIND_LABEL[blocked?.kind ?? "unknown"];
  const left = blocked?.resetsAtMs !== undefined ? blocked.resetsAtMs - nowMs : 0;
  const resets = left > 0 && left <= RESET_MAX_FUTURE_MS ? ` · resets in ${formatSpan(left)}` : "";
  const detail = blocked?.detail ? displayText(blocked.detail, DISPLAY_LIMITS.message) : "";
  return detail ? `${label}${resets} — ${detail}` : `${label}${resets}`;
}
