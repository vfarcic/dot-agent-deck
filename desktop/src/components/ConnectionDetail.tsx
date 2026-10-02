import { DISPLAY_LIMITS, displayText } from "../lib/displayText";

/**
 * The technical half of an incompatible daemon's message — the declared
 * compatibility breaks by name, the protocol number on each side, the two
 * builds — behind a closed disclosure. A user cannot act on any of it, so it
 * stays out of the sentence (CLAUDE.md rule 21); a maintainer reading a bug
 * report still needs it, so it stays one click away rather than only in a log.
 */
export function ConnectionDetail({ detail }: { detail?: string }) {
  if (!detail) return null;
  return (
    <details className="connection-detail" data-testid="connection-detail">
      <summary>Technical details</summary>
      <span>{displayText(detail, DISPLAY_LIMITS.detail)}</span>
    </details>
  );
}
