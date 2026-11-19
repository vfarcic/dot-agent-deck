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
 * rule 23). Past `DECK_ATTENTION_MAX_NAMES` it counts them, with every name
 * and its hold's words on hover. Each name gives its hold's words on hover
 * too, the words its marker gives. It is derived from the snapshot each
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
  if (held.length > DECK_ATTENTION_MAX_NAMES) {
    return (
      <span className="deck-attention" role="status" data-testid="deck-attention" title={held.map((entry) => `${entry.name}: ${entry.words}`).join("\n")}>
        {text}
      </span>
    );
  }
  return (
    <span className="deck-attention" role="status" data-testid="deck-attention">
      {`${INBOX_WORDS.glyph} ${INBOX_WORDS.attentionHeld}: `}
      {held.map((entry, index) => (
        <Fragment key={`${entry.agent.daemonId}:${entry.agent.id}`}>
          {index > 0 && ", "}
          {context ? (
            <button
              type="button"
              className="deck-attention-name"
              title={entry.words}
              aria-label={`Open ${entry.name}`}
              onClick={() => VOICE_ACTIONS.openAgent.run(context, { deckId: entry.agent.daemonId, agentId: entry.agent.id, from, incarnation: entry.agent.incarnation })}
            >{entry.name}</button>
          ) : <span title={entry.words}>{entry.name}</span>}
        </Fragment>
      ))}
    </span>
  );
}
