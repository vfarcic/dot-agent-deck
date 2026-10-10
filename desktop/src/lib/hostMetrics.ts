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

/** Bytes as GiB with at most one decimal, none when it would be `.0` — `128 GiB`, `7.5 GiB`. */
export function formatGib(bytes: number | undefined): string {
  if (bytes === undefined || !Number.isFinite(bytes)) return HOST_UNKNOWN;
  const text = (bytes / GIB).toFixed(1).replace(/\.0$/, "");
  return `${text} GiB`;
}

/** `128 GiB free of 512 GiB total`, with `unknown` for either figure the daemon could not read. */
export function formatDisk(free: number | undefined, total: number | undefined): string {
  return `${formatGib(free)} free of ${formatGib(total)} total`;
}

/** `0.75 across 8 cores`, with each half independently `unknown`. */
export function formatLoad(loadPerCpu: number | undefined, cpuCount: number | undefined): string {
  const load = loadPerCpu === undefined || !Number.isFinite(loadPerCpu) ? HOST_UNKNOWN : loadPerCpu.toFixed(2);
  const cores = cpuCount === undefined ? HOST_UNKNOWN : String(cpuCount);
  return `${load} across ${cores} cores`;
}

/** `1500 ms` — the daemon's own figure, never rounded into something it did not say. */
export function formatSampleAge(ms: number): string {
  return `${Math.max(0, Math.round(ms))} ms`;
}
