import { sameIncarnation } from "./agentKey";
import type { AgentSession, DeckSnapshot, DeckView } from "../types";

type AgentView = Extract<DeckView, { kind: "agent" }>;

/**
 * PRD #1666 part 2 (G-fix6) — `view`, bound to the incarnation of the record
 * it opens, when nothing bound it yet.
 *
 * An opener that has the record in hand (a tile, an overview row) passes its
 * `incarnation` itself, and that value is kept as it is — `undefined`
 * included, which is a deck that named no process. Only a view that carries no
 * `incarnation` key at all (a voice target resolved by number, an initial
 * view) is bound here, from the record `fleet` lists for it at the moment the
 * user asked for it. A view whose agent is not listed yet stays unbound, and
 * is bound by the first record that lists it.
 */
export function bindViewIncarnation(view: DeckView, fleet: readonly DeckSnapshot[]): DeckView {
  if (view.kind !== "agent" || "incarnation" in view) return view;
  const agent = listedAgent(fleet, view);
  return agent ? { ...view, incarnation: agent.incarnation } : view;
}

/** Whether `view` has been bound to an incarnation, `undefined` included. */
export function viewBound(view: AgentView): boolean {
  return "incarnation" in view;
}

/** The agent `fleet` lists under `view`'s `(deckId, agentId)`, whatever its incarnation. */
export function listedAgent(fleet: readonly DeckSnapshot[], view: { deckId: string; agentId: string }): AgentSession | undefined {
  return fleet.find((entry) => entry.connection.deckId === view.deckId)?.agents.find((candidate) => candidate.id === view.agentId);
}

/**
 * The record `view` is FOR in `agents`: the same id AND the incarnation the
 * view was opened on. An unbound view takes the id's record, which is the
 * first record it will be bound to.
 */
export function viewAgent(agents: readonly AgentSession[], view: AgentView): AgentSession | undefined {
  return agents.find((candidate) => candidate.id === view.agentId && (!viewBound(view) || sameIncarnation(view.incarnation, candidate.incarnation)));
}

/**
 * Whether `agents` lists `view`'s id under ANOTHER incarnation than the one
 * the view was opened on: a restarted deck reusing the id for an unrelated
 * agent. The open view's agent is gone.
 */
export function viewAgentReplaced(agents: readonly AgentSession[], view: AgentView): boolean {
  return viewBound(view) && agents.some((candidate) => candidate.id === view.agentId && !sameIncarnation(view.incarnation, candidate.incarnation));
}
