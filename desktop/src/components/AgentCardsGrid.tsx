import { BellRing } from "lucide-react";

import { groupCards, type CardResults, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayText, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { AgentCard, type CardControls } from "./AgentCard";

/**
 * PRD #2374 — the Cards view's line above every daemon's section: how many
 * agents the cards show and how many need the user, the Needs you filter, and
 * what to say when there is nothing to show. `cards` is every card on screen,
 * across the daemons, in the daemons' order.
 */
export function AgentCardsToolbar({ cards, needsYouOnly, controls }: {
  cards: readonly PlacedCard[];
  needsYouOnly: boolean;
  controls: Pick<VoiceActionContext, "setNeedsYouFilter">;
}) {
  const total = cards.length;
  const needYou = cards.filter((card) => card.entry.needs_you).length;
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
    </section>
  );
}

/**
 * PRD #2374 — one daemon's cards, the body of that daemon's section in the
 * Cards view (its header, with the daemon's own controls, stays above it):
 * grouped as the daemon groups them (by the dispatcher that started it, its
 * orchestration, or neither), each group headed by its label and its counts,
 * in the daemon's order. The Needs you filter hides the cards the daemon did
 * not mark.
 */
export function AgentCardsDeckBody({ deckId, cards, needsYouOnly, controls, results }: {
  deckId: string;
  cards: readonly PlacedCard[];
  needsYouOnly: boolean;
  controls: CardControls;
  /** The last answer to each action pressed on a card, to show under its primary button. */
  results?: (card: PlacedCard) => CardResults;
}) {
  const groups = groupCards([...cards], needsYouOnly);
  if (groups.length === 0) return null;
  return (
    <div className="agent-cards-deck" data-testid={`agent-cards-deck-${domIdentity(deckId)}`}>
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
}
