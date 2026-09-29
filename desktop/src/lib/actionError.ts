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
