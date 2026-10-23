import type { AgentSession, DeckFleet } from "../types";
import type { StoppedAgent } from "./agentIdentity";
import { orchestrationTitle } from "./agentNames";

/**
 * PRD #1666 part 2 (G-fix7c) — what a deck listed the last time it was
 * connected, under the daemon process that listed it.
 */
export interface DeckListing {
  incarnation: string | undefined;
  agents: readonly AgentSession[];
}

/** {@link DeckListing} per deck, by `connection.deckId`. */
export type HeldListings = ReadonlyMap<string, DeckListing>;

/**
 * PRD #1666 part 2 (G-fix7c) — read one fleet against what each deck listed
 * last, and answer the agents a deck restart stopped.
 *
 * A connected deck naming another process than the one that listed it last
 * has restarted: every agent that process listed is gone. The fleet already
 * shows only the new process's agents, so nothing is removed here — this
 * only names what went, for the one notice both clients show. A deck that is
 * not connected keeps its last listing, so a restart seen across a
 * disconnection is still one; a reconnect to the SAME process names nothing;
 * a deck that named no process before or now names nothing either, since
 * there is no telling a restart from a quiet deck. A deck that left the fleet
 * is forgotten.
 *
 * Nothing here sends anything anywhere.
 */
export function observeRestarts(held: HeldListings, fleet: DeckFleet): { held: Map<string, DeckListing>; stopped: StoppedAgent[] } {
  const next = new Map<string, DeckListing>();
  const stopped: StoppedAgent[] = [];
  for (const deck of fleet) {
    const deckId = deck.connection.deckId;
    if (deckId === undefined) continue;
    const before = held.get(deckId);
    if (deck.connection.status !== "connected") {
      if (before) next.set(deckId, before);
      continue;
    }
    const process = deck.connection.incarnation;
    if (before?.incarnation !== undefined && process !== undefined && before.incarnation !== process) {
      // Every agent of a listing carries the listing's process, so all of them.
      stopped.push(...before.agents.map(stoppedAgent));
    }
    next.set(deckId, { incarnation: process, agents: deck.agents });
  }
  return { held: next, stopped };
}

/** How the notice names `agent`: its label, and its role and run when it is an orchestration's. */
export function stoppedAgent(agent: AgentSession): StoppedAgent {
  if (agent.tab.kind !== "orchestration") return { label: agent.displayName };
  return { label: agent.displayName, role: agent.tab.roleName, orchestration: orchestrationTitle(agent.tab) };
}
