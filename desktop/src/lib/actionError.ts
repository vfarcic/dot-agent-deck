import type { StartDaemonFailure } from "../types";

/**
 * PRD #1223 audit F6 — the one `desktop_run_action` rejection that is not a
 * bare string.
 *
 * `desktop_run_action` rejects with the crate's error sentence for every
 * failure, which is what every `catch` in the webview reads. The exception is a
 * launch that failed AND whose rollback could not confirm that every role it
 * touched is stopped: the crate then rejects with `{ message, unconfirmedStops }`
 * (`DesktopActionError::LaunchCleanup` in `src-tauri/src/dto.rs`), so the
 * roles that may still be running arrive as data. In the prose they are the
 * last clause — after the primary error and every started role's name — and so
 * the first thing a display clamp cuts; the dialog shows them on their own,
 * before the clamped sentence, rather than parsing them back out of it.
 */
export class LaunchCleanupError extends Error {
  /** Every role the launch could not confirm is stopped. Never empty. */
  readonly unconfirmedStops: readonly string[];

  constructor(message: string, unconfirmedStops: readonly string[]) {
    super(message);
    this.name = "LaunchCleanupError";
    this.unconfirmedStops = unconfirmedStops;
  }
}

/**
 * What a `desktop_run_action` rejection should be rethrown as: a
 * {@link LaunchCleanupError} for the crate's structured launch failure, and the
 * rejection itself, untouched, for everything else — so no existing caller
 * sees a different value than it did before the structured shape existed.
 */
export function actionErrorFrom(cause: unknown): unknown {
  if (typeof cause !== "object" || cause === null || cause instanceof Error) return cause;
  const record = cause as Record<string, unknown>;
  const stops = record.unconfirmedStops;
  if (typeof record.message !== "string" || !Array.isArray(stops) || stops.length === 0) return cause;
  const names = stops.filter((stop): stop is string => typeof stop === "string");
  if (names.length === 0) return cause;
  return new LaunchCleanupError(record.message, names);
}

/**
 * Issue #1490 — a Start daemon that failed. `desktop_start_daemon` rejects
 * with `{ message, failure, detail }` for a start that failed
 * (`DesktopStartDaemonError::Failed` in `src-tauri/src/dto.rs`), so the
 * technical half — the spawn error, what ssh printed — reaches a disclosure
 * instead of being dropped (PR #1623 review). Every other rejection is the
 * bare sentence it always was.
 */
export class StartDaemonError extends Error {
  /** What kind of problem stopped the start. */
  readonly failure?: StartDaemonFailure;
  /** The technical half, for a disclosure. */
  readonly detail?: string;

  constructor(message: string, failure?: StartDaemonFailure, detail?: string) {
    super(message);
    this.name = "StartDaemonError";
    this.failure = failure;
    this.detail = detail;
  }
}

/**
 * What a `desktop_start_daemon` rejection should be rethrown as: a
 * {@link StartDaemonError} for the crate's structured start failure, and an
 * `Error` carrying the sentence for everything else.
 */
export function startDaemonErrorFrom(cause: unknown): Error {
  if (cause instanceof Error) return cause;
  if (typeof cause === "object" && cause !== null) {
    const record = cause as Record<string, unknown>;
    if (typeof record.message === "string") {
      const failure = typeof record.failure === "string" ? record.failure as StartDaemonFailure : undefined;
      const detail = typeof record.detail === "string" && record.detail.trim() !== "" ? record.detail : undefined;
      return new StartDaemonError(record.message, failure, detail);
    }
  }
  return new Error(String(cause));
}
