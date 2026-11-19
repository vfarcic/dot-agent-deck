import { Fragment } from "react";
import { DECK_ATTENTION_MAX_NAMES, deckAttentionNamesText, heldAgents, INBOX_WORDS } from "../lib/inboxWords";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { AgentSession } from "../types";

/**
 * Issue #2387: the header's deck-wide attention item, inline in the top bar
 * of the deck and the overview alike. It names the agents whose tile or row
 * carries the `held` marker (`heldAgents`) — exactly those, on every daemon
 * the screen shows — and each name opens its agent through `openAgent`, the
 * registry entry "open the tester agent" already reaches by voice (CLAUDE.md
 * rule 23). Past `DECK_ATTENTION_MAX_NAMES` it counts them. The item gives
 * every held agent and its hold's words on hover, and each name its own;
 * each name is cut to a width of its own, so the top bar can shorten a name
 * but never drop one. It is derived from the snapshot each
 * render, so it survives a reconnect and a change of selection or screen.
 *
 * Without `context` (a host that cannot navigate) the names are plain text.
 */
export function DeckAttention({ agents, from, context }: {
  agents: readonly AgentSession[];
  from: "deck" | "overview";
  context?: Pick<VoiceActionContext, "navigate"> & Partial<Pick<VoiceActionContext, "selectAgent" | "revealAgent">>;
}) {
  const held = heldAgents(agents);
  const text = deckAttentionNamesText(held.map((entry) => entry.name));
  if (text === undefined) return null;
  /* Every held agent and its reason, on hover over the item itself: a name
     cut short by the top bar's width is still read whole here. */
  const list = held.map((entry) => `${entry.fullName}: ${entry.words}`).join("\n");
  if (held.length > DECK_ATTENTION_MAX_NAMES) {
    return (
      <span className="deck-attention" role="status" data-testid="deck-attention" title={list}>
        {text}
      </span>
    );
  }
  return (
    <span className="deck-attention" role="status" data-testid="deck-attention" title={list}>
      {`${INBOX_WORDS.glyph} ${INBOX_WORDS.attentionHeld}: `}
      {held.map((entry, index) => (
        <Fragment key={`${entry.agent.daemonId}:${entry.agent.id}`}>
          {index > 0 && ", "}
          {context ? (
            <button
              type="button"
              className="deck-attention-name"
              title={`${entry.fullName}: ${entry.words}`}
              aria-label={`Open ${entry.fullName}`}
              onClick={() => VOICE_ACTIONS.openAgent.run(context, { deckId: entry.agent.daemonId, agentId: entry.agent.id, from, incarnation: entry.agent.incarnation })}
            >{entry.name}</button>
          ) : <span className="deck-attention-name" title={`${entry.fullName}: ${entry.words}`}>{entry.name}</span>}
        </Fragment>
      ))}
    </span>
  );
}
