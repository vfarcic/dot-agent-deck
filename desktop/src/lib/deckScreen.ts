import type { DeckSnapshot } from "../types";

/**
 * The snapshot the deck screen renders from, given whether **All Decks** is
 * selected (#1083).
 *
 * Under All Decks the crate still hands the deck screen "the selected deck's"
 * snapshot, and that is the local deck's: the selection resolves there because
 * the watcher and the tunnels need an endpoint, not because the user chose it.
 * The deck screen cannot merge across decks — every tile owns a terminal — so
 * it shows "Select a deck" instead, and this is what keeps the local deck out
 * of everything else on that screen too: no tile, so no terminal is declared
 * shown; no `1`–`4` or palette focus target; no evidence, handoff or run-graph
 * node; and no run id or project name in the top bar taken from the local deck.
 *
 * The connection is kept, because the Deck selector and the banner read it, and
 * so are the profiles, which are this device's drafts rather than any deck's.
 * The same object comes back when nothing is stripped, so a memo over it is
 * stable across renders.
 */
export function deckScreenSnapshot(snapshot: DeckSnapshot, allDecks: boolean): DeckSnapshot {
  if (!allDecks) return snapshot;
  return {
    ...snapshot,
    runId: "—",
    repo: "All daemons",
    worktree: "",
    branch: undefined,
    stages: [],
    agents: [],
    evidence: [],
    handoffs: [],
  };
}
