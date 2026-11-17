import { BellRing } from "lucide-react";

import { actionLabel, cardStatusLabel, deliverySentence, disabledReasonSentence, resultSentence, type CardResults, fieldValueText, multilineText, needsYouSentence, placedCardKey, primaryAction, promptPreview, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";

/** The members a card's own controls dispatch through. */
export type CardControls = Pick<VoiceActionContext, "showAgentCard" | "runCardAction">;

/** The card a control names: its deck, agent and the daemon process that listed it. */
export function cardRef(card: PlacedCard): { deckId: string; agentId: string; incarnation: string } {
  return { deckId: card.deckId, agentId: card.entry.agent_id, incarnation: card.entry.instance_id };
}

/** A small label saying who a piece of text comes from: the agent's own word, or something the deck checked. */
export function SourceBadge({ source }: { source: "agent" | "deck" }) {
  return source === "agent"
    ? <span className="card-source is-agent" title="Written by the agent; the deck did not check it">agent says</span>
    : <span className="card-source is-deck" title="Checked by the deck">deck checked</span>;
}

/**
 * PRD #2374 — one agent's card on the dashboard's Cards view: who it is, its
 * deck-checked status, whether it needs the user, what it last reported, its
 * card fields and its primary action. Everything comes from the daemon's
 * entry as received; nothing here decides status, needs-you or which action
 * is primary.
 */
export function AgentCard({ card, controls, results }: { card: PlacedCard; controls: CardControls; results?: CardResults }) {
  const { entry } = card;
  const name = displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id);
  const primary = primaryAction(entry);
  const fields = entry.fields.filter((field) => field.placement === "card" || field.placement === "compact");
  const needsYou = needsYouSentence(entry);
  const summary = entry.report?.summary ? displayText(entry.report.summary, DISPLAY_LIMITS.message) : undefined;
  const primaryTitle = primary && !primary.enabled ? `Disabled: ${disabledReasonSentence(primary.disabled_reason)}` : undefined;
  return (
    <article
      className={`agent-card${entry.needs_you ? " needs-you" : ""}`}
      data-testid={`agent-card-${domIdentity(placedCardKey(card))}`}
      data-card-key={entry.key}
      data-status={entry.status}
      aria-label={`${name}${entry.needs_you ? ", needs you" : ""}`}
    >
      <header className="agent-card-header">
        <button
          type="button"
          className="agent-card-name"
          data-testid="agent-card-show"
          aria-label={`Show ${name}'s card`}
          title={`Show ${name}'s card`}
          onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, cardRef(card))}
        >{name}</button>
        <span className={`status-label card-status status-${domIdentity(entry.status)}`} title="Status checked by the deck">{cardStatusLabel(entry.status)}</span>
        {entry.needs_you && <span className="agent-card-needs-you" data-testid="agent-card-needs-you" title={needsYou}><BellRing size={11} aria-hidden="true" />Needs you</span>}
      </header>
      {summary && <p className="agent-card-summary"><SourceBadge source="agent" /> <span>{summary}</span></p>}
      {entry.report === null && <p className="agent-card-summary is-empty">No report from this agent yet.</p>}
      {fields.length > 0 && (
        <dl className="agent-card-fields">
          {fields.map((field) => (
            <div key={field.name} className="agent-card-field" data-field={field.name}>
              <dt title={field.description ? displayTitle(field.description) : undefined}>{displayText(field.label, DISPLAY_LIMITS.name)}</dt>
              <dd title={displayTitle(fieldValueText(field.value))}>{displayText(fieldValueText(field.value), DISPLAY_LIMITS.prompt)}</dd>
            </div>
          ))}
        </dl>
      )}
      {primary && (
        <div className="agent-card-primary">
          <button
            type="button"
            className="button primary compact"
            data-testid="agent-card-primary"
            disabled={!primary.enabled}
            title={primaryTitle}
            onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: primary.id })}
          >{displayText(actionLabel(entry, primary), DISPLAY_LIMITS.name)}</button>
          {primary.prompt && <small className="agent-card-prompt" title={multilineText(primary.prompt)}>{promptPreview(displayText(primary.prompt, DISPLAY_LIMITS.detail))}</small>}
          {!primary.enabled && <small className="agent-card-disabled">{disabledReasonSentence(primary.disabled_reason)}</small>}
          {results?.[primary.id] && <small className="agent-card-result" role="status" data-testid="agent-card-result">{displayText(resultSentence(results[primary.id]), DISPLAY_LIMITS.detail)}</small>}
          {primary.last_delivery && <small className="agent-card-delivery">{displayText(deliverySentence(primary.last_delivery), DISPLAY_LIMITS.message)}</small>}
        </div>
      )}
    </article>
  );
}
