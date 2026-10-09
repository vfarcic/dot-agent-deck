import { DISPLAY_LIMITS, displayText } from "../lib/displayText";

/**
 * The technical half of an incompatible daemon's message — the declared
 * compatibility breaks by name, the protocol number on each side, the two
 * builds — behind a closed disclosure. A user cannot act on any of it, so it
 * stays out of the sentence (CLAUDE.md rule 21); a maintainer reading a bug
 * report still needs it, so it stays one click away rather than only in a log.
 *
 * `detail` may be several strings (PR #1623 review: a disconnected deck's
 * connection error beside its reason's detail); each is its own line.
 */
export function ConnectionDetail({ detail }: { detail?: string | readonly string[] }) {
  const lines = (typeof detail === "string" ? [detail] : detail ?? []).filter((line) => line.trim() !== "");
  if (lines.length === 0) return null;
  return (
    <details className="connection-detail" data-testid="connection-detail">
      <summary>Technical details</summary>
      {lines.map((line, index) => <span key={index}>{displayText(line, DISPLAY_LIMITS.detail)}</span>)}
    </details>
  );
}
