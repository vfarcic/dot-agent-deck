/**
 * PRD #1258 M4 — the words and number formats the deck card uses for a deck's
 * host. They match the TUI's host overlay (`src/ui.rs`, `host_metrics_lines`)
 * word for word, because a user who reads one client carries the vocabulary to
 * the other (CLAUDE.md rule 22).
 */

export const HOST_TITLE = "Host of this deck";
export const HOST_SUBTITLE = "The machine this deck's daemon runs on";
export const HOST_UNKNOWN = "unknown";
export const HOST_NOT_AVAILABLE = "Host metrics are not available from this deck.";
export const HOST_NOT_AVAILABLE_WHY = "Its daemon is an older version, or runs on Windows.";

const GIB = 1024 ** 3;

/** The label a watched role reads as; a role a newer daemon added shows under its own name. */
export function hostRoleLabel(role: string): string {
  switch (role) {
    case "working_root": return "Working root";
    case "worktree_parent": return "Worktree parent";
    case "temp_root": return "Temp root";
    default: return role;
  }
}

/**
 * The largest age either client shows, in ms: the largest integer a JS number
 * holds exactly. The daemon's age is a u64; above this the webview could not
 * print the figure the TUI prints, so both clients stop here. The TUI's
 * `HOST_METRICS_MAX_SHOWN_AGE_MS` is the same number.
 */
export const HOST_MAX_SHOWN_AGE_MS = Number.MAX_SAFE_INTEGER;

/**
 * `value` with `decimals` places, written exactly as the TUI's Rust
 * `format!("{:.N}")` writes it — which `toFixed` does not in three cases:
 *
 * - an exact tie rounds to the even neighbour (`0.125` → `0.12`), where
 *   `toFixed` rounds away from zero (`0.13`). Ties are common here: a load of
 *   `1.00` on 8 cores is exactly `0.125` per core, and `1.25 GiB` is exact.
 * - negative zero keeps its sign (`-0.00`).
 * - from `1e21` up every digit is written, where `toFixed` switches to
 *   exponent notation.
 *
 * Exported for its test; the formatters below are its only callers.
 */
export function fixedLikeRust(value: number, decimals: number): string {
  const sign = value < 0 || Object.is(value, -0) ? "-" : "";
  const abs = Math.abs(value);
  if (abs >= 1e21) {
    // A double this large is an integer, so BigInt holds it exactly.
    return `${sign}${BigInt(abs).toString()}${decimals > 0 ? `.${"0".repeat(decimals)}` : ""}`;
  }
  let text = abs.toFixed(decimals);
  // A double sits exactly halfway between two `decimals`-place numbers only
  // when it is an odd multiple of 2^-(decimals+1); scaling by a power of two is
  // exact, so this test is too. `toFixed` then took the upper neighbour, and
  // when that ends in an odd digit the even one is a single digit below it —
  // an odd digit is at least 1, so no borrow is needed.
  const scaled = abs * 2 ** (decimals + 1);
  if (Number.isInteger(scaled) && scaled % 2 === 1) {
    const last = text.charCodeAt(text.length - 1) - 48;
    if (last % 2 === 1) text = `${text.slice(0, -1)}${last - 1}`;
  }
  return `${sign}${text}`;
}

/** Bytes as GiB with at most one decimal, none when it would be `.0` — `128 GiB`, `7.5 GiB`. */
export function formatGib(bytes: number | undefined): string {
  if (bytes === undefined || !Number.isFinite(bytes)) return HOST_UNKNOWN;
  const text = fixedLikeRust(bytes / GIB, 1).replace(/\.0$/, "");
  return `${text} GiB`;
}

/** `128 GiB free of 512 GiB total`, with `unknown` for either figure the daemon could not read. */
export function formatDisk(free: number | undefined, total: number | undefined): string {
  return `${formatGib(free)} free of ${formatGib(total)} total`;
}

/** `0.75 across 8 cores`, with each half independently `unknown`. */
export function formatLoad(loadPerCpu: number | undefined, cpuCount: number | undefined): string {
  const load = loadPerCpu === undefined || !Number.isFinite(loadPerCpu) ? HOST_UNKNOWN : fixedLikeRust(loadPerCpu, 2);
  const cores = cpuCount === undefined ? HOST_UNKNOWN : String(cpuCount);
  return `${load} across ${cores} cores`;
}

/** `1500 ms` — the daemon's own figure, never rounded into something it did not say, up to {@link HOST_MAX_SHOWN_AGE_MS}. */
export function formatSampleAge(ms: number): string {
  return `${Math.min(HOST_MAX_SHOWN_AGE_MS, Math.max(0, Math.round(ms)))} ms`;
}
