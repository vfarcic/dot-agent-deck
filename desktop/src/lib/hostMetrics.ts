import type { HostLevel, HostMetricsReport } from "../types";

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
/** The deck header's host button text. */
export const HOST_BUTTON_TEXT = "Host";

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

const LEVELS: readonly HostLevel[] = ["ok", "warn", "critical"];

/**
 * `level` when it is one this app knows, else `undefined` — a level a newer
 * daemon added reads as no level, so the value shows uncoloured rather than
 * failing or claiming `ok`. The bridge already drops one it cannot decode;
 * this keeps the webview honest whatever reaches it.
 */
export function hostLevel(level: unknown): HostLevel | undefined {
  return LEVELS.find((known) => known === level);
}

/**
 * The word written beside a non-ok value, so colour is never the only signal.
 * The TUI's `host_metrics_level_word` returns the same words (CLAUDE.md rule 22).
 */
export function hostLevelWord(level: unknown): string | undefined {
  switch (hostLevel(level)) {
    case "warn": return "(warning)";
    case "critical": return "(critical)";
    default: return undefined;
  }
}

/** The worst level among a deck's readings, or `undefined` when none carries one. */
export function worstHostLevel(report: HostMetricsReport): HostLevel | undefined {
  if (report.status !== "available") return undefined;
  const { disks, loadLevel, memoryLevel } = report.metrics;
  let worst: HostLevel | undefined;
  for (const candidate of [...disks.map((disk) => disk.level), loadLevel, memoryLevel]) {
    const level = hostLevel(candidate);
    if (level !== undefined && (worst === undefined || LEVELS.indexOf(level) > LEVELS.indexOf(worst))) worst = level;
  }
  return worst;
}

/**
 * The deck's host state in words, for the button's accessible name and
 * tooltip: the worst level, or that the deck does not report its host.
 * `undefined` when the deck reports numbers but no levels.
 */
export function hostStateWords(report: HostMetricsReport): string | undefined {
  if (report.status !== "available") return "not available from this deck";
  switch (worstHostLevel(report)) {
    case "critical": return "critical";
    case "warn": return "warning";
    case "ok": return "ok";
    default: return undefined;
  }
}

/**
 * PR #1672 — one daemon section of the dashboard, as the Host voice command
 * sees it: its key, the name its header shows, and whether its Host button is
 * on screen (connected, with figures from its daemon).
 */
export interface HostDeckChoice {
  deckId: string | undefined;
  name: string;
  connected: boolean;
  hasFigures: boolean;
}

/** No daemon on the dashboard shows a Host button. */
export const HOST_VOICE_NONE = "No daemon on the dashboard has Host figures to show.";
/** A daemon was named that the dashboard does not show. */
export const HOST_VOICE_NOT_SHOWN = "That daemon is not on the dashboard, so its Host figures cannot be shown.";

/** Several daemons show a Host button and none was named. */
export function hostVoiceWhich(names: readonly string[]): string {
  return `Several daemons are on the dashboard: say which one, like “host ${names[0]}”.`;
}

/**
 * PR #1672 — which daemon's Host popover a "show host" opens. A named daemon
 * (`deckId`, what the row's `deck_ref` resolved to, or the button's own) opens
 * when its Host button is on screen, and is refused in words otherwise. With
 * none named, the one daemon showing a Host button opens; with several, the
 * user is asked to name one rather than one being guessed.
 */
export function chooseHostDeck(decks: readonly HostDeckChoice[], deckId: string | undefined): { open: string } | { refused: string } {
  if (deckId !== undefined) {
    const deck = decks.find((candidate) => candidate.deckId === deckId);
    if (!deck) return { refused: HOST_VOICE_NOT_SHOWN };
    if (!deck.connected) return { refused: `“${deck.name}” is not connected, so it has no Host figures to show.` };
    if (!deck.hasFigures) return { refused: `“${deck.name}” has not sent its Host figures yet.` };
    return { open: deckId };
  }
  const shown = decks.filter((deck) => deck.deckId !== undefined && deck.connected && deck.hasFigures);
  if (shown.length === 0) return { refused: HOST_VOICE_NONE };
  if (shown.length > 1) return { refused: hostVoiceWhich(shown.map((deck) => deck.name)) };
  return { open: shown[0].deckId! };
}
