import { BellRing } from "lucide-react";

import { groupCards, type CardResults, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayText, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { AgentCard, type CardControls } from "./AgentCard";

/** One deck's cards, in the daemon's display order. */
export interface CardsDeck {
  deckId: string;
  /** The deck's name, shown above its groups when more than one deck is on screen. */
  name: string;
  cards: PlacedCard[];
}

/**
 * PRD #2374 — the dashboard's Cards view: every agent's card, grouped as the
 * daemon groups them (by the dispatcher that started it, its orchestration, or
 * neither), each group headed by its label and its counts, in the daemon's
 * order. The Needs you filter hides the cards the daemon did not mark.
 */
export function AgentCardsGrid({ decks, needsYouOnly, controls, results }: {
  decks: readonly CardsDeck[];
  needsYouOnly: boolean;
  controls: CardControls & Pick<VoiceActionContext, "setNeedsYouFilter">;
  /** The last answer to each action pressed on a card, to show under its primary button. */
  results?: (card: PlacedCard) => CardResults;
}) {
  const total = decks.reduce((sum, deck) => sum + deck.cards.length, 0);
  const needYou = decks.reduce((sum, deck) => sum + deck.cards.filter((card) => card.entry.needs_you).length, 0);
  return (
    <section className="agent-cards" aria-label="Agent cards" data-testid="agent-cards">
      <div className="agent-cards-toolbar">
        <span className="agent-cards-count">{total} {total === 1 ? "agent" : "agents"} · {needYou} {needYou === 1 ? "needs" : "need"} you</span>
        <button
          type="button"
          className={`button secondary compact agent-cards-filter${needsYouOnly ? " is-on" : ""}`}
          data-testid="agent-cards-needs-you-filter"
          aria-pressed={needsYouOnly}
          title={needsYouOnly ? "Show every card" : "Show only the cards that need you"}
          onClick={() => VOICE_ACTIONS.toggleNeedsYouFilter.run(controls, { needsYouOnly: !needsYouOnly })}
        ><BellRing size={13} aria-hidden="true" /><span>Needs you</span></button>
      </div>
      {total === 0 && <p className="agent-cards-empty">No agent has a card yet. Every agent the daemon runs gets one.</p>}
      {total > 0 && needsYouOnly && needYou === 0 && <p className="agent-cards-empty" data-testid="agent-cards-none-need-you">No card needs you right now.</p>}
      {decks.map((deck) => {
        const groups = groupCards(deck.cards, needsYouOnly);
        if (groups.length === 0) return null;
        return (
          <div className="agent-cards-deck" key={deck.deckId} data-testid={`agent-cards-deck-${domIdentity(deck.deckId)}`}>
            {decks.length > 1 && <h2 className="agent-cards-deck-name">{displayText(deck.name, DISPLAY_LIMITS.name)}</h2>}
            {groups.map((group) => (
              <section className="agent-cards-group" key={group.key} data-testid="agent-cards-group" data-group-kind={group.kind} aria-label={group.label}>
                <h3 className="agent-cards-group-heading">
                  <span className="section-kicker">{group.kind === "standalone" ? "Standalone" : group.kind === "dispatcher" ? "Dispatcher" : group.kind === "orchestration" ? "Orchestration" : displayText(group.kind, DISPLAY_LIMITS.name)}</span>
                  <span className="agent-cards-group-label">{displayText(group.label, DISPLAY_LIMITS.name)}</span>
                  <span className="agent-cards-group-count" data-testid="agent-cards-group-count">{group.total} {group.total === 1 ? "agent" : "agents"} · {group.needYou} {group.needYou === 1 ? "needs" : "need"} you</span>
                </h3>
                <div className="agent-cards-grid">
                  {group.cards.map((card) => <AgentCard key={`${card.entry.instance_id}\u0000${card.entry.key}`} card={card} controls={controls} results={results?.(card)} />)}
                </div>
              </section>
            ))}
          </div>
        );
      })}
    </section>
  );
}
