import type { ConnectionView, DeckDirectoryEntry, DeckFleet } from "../types";
import { DISPLAY_LIMITS, deckName, displayText } from "./displayText";

/**
 * PRD #1223 M4/M5 — the rules of the New agent flow, kept out of the component
 * so each one is testable as a function.
 */

/**
 * How long the flow waits, after a deck accepted a start, for that deck's fleet
 * entry to list the new agent before it gives up on opening the pane (PRD #1223
 * M5).
 *
 * The pane cannot open earlier: `paneAgentRetired` in `App.tsx` closes a pane
 * whose connected deck does not list its agent. The start action refreshes the
 * target deck directly and nudges its watcher, so the ordinary case is one
 * round trip. The bound is for the slow case, and it is set against the
 * crate's five-second `RECONCILE_INTERVAL` — the longest a watched deck takes to
 * re-read its agent list when nothing else prompts it — with room on top for a
 * remote deck's tunnel.
 */
export const NEW_AGENT_APPEAR_TIMEOUT_MS = 12_000;

/**
 * What a deck in each state that cannot take a spawn says when its connection
 * carries no message of its own. `pending`, `unconfigured`, `disconnected` and
 * `incompatible` are the sentences the overview's degraded group notes fall
 * back to, and those notes read them from here, so the deck step and the
 * overview cannot drift; `loading` is the deck step's short form of the
 * overview's loading note, which carries more than one sentence.
 */
export const DECK_STATE_FALLBACK = {
  pending: "This deck has not reported yet.",
  loading: "Reading the deck's agent list.",
  unconfigured: "This deck has no address yet.",
  disconnected: "No deck is listening on the configured socket.",
  incompatible: "A deck answered but this build cannot speak to it.",
} as const;

/**
 * Why a deck cannot take a spawn, as display text — or `undefined` when it can.
 *
 * A deck can take one when it is connected, which already excludes a pending
 * deck (`loading`), an unconfigured one and an incompatible one (`error`). A
 * deck whose build stamps differ is `error` until the user accepts it through
 * the overview's Connect anyway, and `connected` afterwards — so "compatible
 * or explicitly accepted" is the connected status, with no second flag to read.
 */
export function deckUnavailableReason(connection: ConnectionView): string | undefined {
  const own = connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined;
  if (connection.pending) return own ?? DECK_STATE_FALLBACK.pending;
  if (connection.unconfigured) return own ?? DECK_STATE_FALLBACK.unconfigured;
  switch (connection.status) {
    case "connected":
      return undefined;
    case "loading":
      return own ?? DECK_STATE_FALLBACK.loading;
    case "disconnected":
      return own ?? DECK_STATE_FALLBACK.disconnected;
    case "error":
      return own ?? DECK_STATE_FALLBACK.incompatible;
  }
}

/** One row of the deck step. */
export interface DeckChoice {
  /** The wire `connection.deckId` — the value every later request carries. */
  deckId: string;
  /** What the deck is called, as display text. */
  name: string;
  deckKind: "local" | "remote";
  /** Why it cannot take a spawn; absent when it can. */
  reason?: string;
}

/**
 * Every deck in the fleet, in fleet order, with the reason each ineligible one
 * gives. An entry with no `deckId` is a placeholder for a fleet that has not
 * arrived, not a deck, and is left out.
 */
export function deckChoices(fleet: DeckFleet): DeckChoice[] {
  return fleet.flatMap((deck) => {
    const deckId = deck.connection.deckId;
    if (deckId === undefined) return [];
    const reason = deckUnavailableReason(deck.connection);
    return [{ deckId, name: deckName(deck.connection), deckKind: deck.connection.deckKind ?? "local", ...(reason === undefined ? {} : { reason }) }];
  });
}

/**
 * The deck the step opens on: the one the flow was opened FROM (a deck
 * header's affordance) when it can take a spawn, otherwise the only eligible
 * deck when there is exactly one, otherwise none — the user chooses.
 */
export function preselectedDeck(choices: readonly DeckChoice[], requested?: string): string | undefined {
  if (requested !== undefined && choices.some((choice) => choice.deckId === requested && choice.reason === undefined)) return requested;
  const eligible = choices.filter((choice) => choice.reason === undefined);
  return eligible.length === 1 ? eligible[0].deckId : undefined;
}

/**
 * The Name field's prefill: the last component of a path the DECK returned —
 * a label, never a path, and never sent back as one. Both separators are
 * accepted because the deck's platform need not be this one. The root, which
 * has no last component, gives an empty name, as the TUI's `file_name()` does.
 */
export function directoryLabel(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).at(-1) ?? "";
}

/**
 * The Command field's prefill, in the TUI's order (`resolve_seed_command`):
 * the deck host's configured `default_command`, then the command this app last
 * started a plain agent with on that deck, then blank — which starts the
 * deck's default shell.
 */
export function seedCommand(defaultCommand?: string, lastCommand?: string): string {
  if (defaultCommand) return defaultCommand;
  if (lastCommand?.trim()) return lastCommand;
  return "";
}

/**
 * Whether a refusal means the chosen deck has left the fleet — the crate's
 * `DeckScope::resolve` wording, which the fixture bridge repeats. The flow
 * returns to the deck step on it rather than retargeting another deck.
 */
export function isDeckGoneError(message: string): boolean {
  return message.includes("that deck is not one this app is observing");
}

/** Whether `deckId`'s fleet entry lists `agentId` — the composite identity, never the bare id. */
export function fleetLists(fleet: DeckFleet, deckId: string, agentId: string): boolean {
  return fleet.some((deck) => deck.connection.deckId === deckId && deck.agents.some((agent) => agent.id === agentId));
}

/** The directory step's filter: a case-insensitive substring of the entry's name, as the TUI picker's `refilter`. */
export function filterDirectoryEntries(entries: readonly DeckDirectoryEntry[], filter: string): DeckDirectoryEntry[] {
  const query = filter.toLowerCase();
  return query ? entries.filter((entry) => entry.displayName.toLowerCase().includes(query)) : [...entries];
}

/** The subset of a `KeyboardEvent` {@link isNewAgentShortcut} reads, so a test can pass a plain object. */
export interface ShortcutKeyEvent {
  key: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

/**
 * Ctrl+N, or Cmd+N — the TUI's `Ctrl+n`. Either modifier on every platform, for
 * `zoomIntentFromKey`'s reason: no other binding here uses `Ctrl N` on macOS or
 * `Cmd N` elsewhere, so reading the platform to pick one buys nothing. Alt and
 * Shift are excluded, so `Ctrl Shift N` and `Alt Cmd N` stay the platform's.
 */
export function isNewAgentShortcut(event: ShortcutKeyEvent): boolean {
  return (event.ctrlKey === true || event.metaKey === true) && event.altKey !== true && event.shiftKey !== true && event.key.toLowerCase() === "n";
}
