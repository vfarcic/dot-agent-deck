import type { EndedRole } from "../types";
import { ENDED_ROLE_WORDS } from "../lib/endedRoles";
import { BellRing } from "lucide-react";

import { actionLabel, cardStatusLabel, deliverySentence, disabledReasonSentence, exactTextSegments, exactTextString, hasMarkers, MARKER_CLOSE, MARKER_OPEN, markerTitle, resultSentence, type CardResults, fieldValueText, needsYouSentence, placedCardKey, primaryAction, primaryNeedsReview, promptPreview, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";

/** The members a card's own controls dispatch through. */
export type CardControls = Pick<VoiceActionContext, "showAgentCard" | "runCardAction">;

/** The card a control names: its deck, agent and the daemon process that listed it. */
export function cardRef(card: PlacedCard): { deckId: string; agentId: string; incarnation: string } {
  return { deckId: card.deckId, agentId: card.entry.agent_id, incarnation: card.entry.instance_id };
}

/** The agent-source label at the head of a card's fields, shown whether or not the card has a summary (review S2). */
function SourceBadgeRow() {
  return <div className="agent-card-fields-source" data-testid="agent-card-fields-source"><SourceBadge source="agent" /></div>;
}

/** A small label saying who a piece of text comes from: the agent's own word, or something the deck checked. */
export function SourceBadge({ source }: { source: "agent" | "deck" }) {
  return source === "agent"
    ? <span className="card-source is-agent" title="Written by the agent; the deck did not check it">agent says</span>
    : <span className="card-source is-deck" title="Checked by the deck">deck checked</span>;
}

/**
 * The exact text a prompt action sends, as it is sent: line breaks kept, and
 * every invisible or reordering character shown as a marker, `⟦…⟧`, styled
 * apart from the text (a boxed code) rather than dropped or applied (audit
 * F3, A3; review R3). Rendered as text, never as markup.
 */
export function ExactText({ text }: { text: string }) {
  return (
    <>
      {exactTextSegments(text).map((segment, index) => (segment.escaped
        ? <span key={index} className="exact-text-escape" title={markerTitle(segment.raw)}>{segment.shown}</span>
        : <span key={index}>{segment.shown}</span>))}
    </>
  );
}

/** What the markers in a shown prompt mean, said under it when it has any. */
const MARKER_NOTE = `${MARKER_OPEN}…${MARKER_CLOSE} marks a character you cannot see; it is sent as is.`;

/** {@link MARKER_NOTE}, when `text` shows any marker. */
export function ExactTextNote({ text }: { text: string }) {
  return hasMarkers(text) ? <small className="exact-text-note" data-testid="exact-text-note">{MARKER_NOTE}</small> : null;
}

/**
 * PRD #2374 — one agent's card on the dashboard's Cards view: who it is, its
 * deck-checked status, whether it needs the user, what it last reported, its
 * card fields and its primary action. Everything comes from the daemon's
 * entry as received; nothing here decides status, needs-you or which action
 * is primary.
 */
export function AgentCard({ card, controls, results, ended }: { card: PlacedCard; controls: CardControls; results?: CardResults; ended?: EndedRole }) {
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
      {/* Issue #1965: an exited orchestration role says why, in the words its table row and the TUI's card use. */}
      {ended && (
        <div className="agent-card-ended" data-testid="agent-card-ended" title={displayText(ended.headline, DISPLAY_LIMITS.title)}>
          <p className="agent-card-ended-reason">{ENDED_ROLE_WORDS.status}: {displayText(ended.reason, DISPLAY_LIMITS.title)}</p>
          {ended.lastOutput.length > 0 && (
            <>
              <span className="agent-card-ended-label">{ENDED_ROLE_WORDS.lastOutput}:</span>
              <pre>{ended.lastOutput.map((line) => displayText(line, DISPLAY_LIMITS.title)).join("\n")}</pre>
            </>
          )}
        </div>
      )}
      {summary && <p className="agent-card-summary"><SourceBadge source="agent" /> <span>{summary}</span></p>}
      {entry.report === null && <p className="agent-card-summary is-empty">No report from this agent yet.</p>}
      {fields.length > 0 && <SourceBadgeRow />}
      {fields.length > 0 && (
        <dl className="agent-card-fields" aria-label="Fields the agent reported">
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
            onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: primary.id, cardActionToken: primary.token, ...(primaryNeedsReview(primary) ? { cardActionReview: true } : {}) })}
          >{displayText(actionLabel(entry, primary), DISPLAY_LIMITS.name)}</button>
          {primary.source === "agent" && <span data-testid="agent-card-primary-source"><SourceBadge source="agent" /></span>}
          {primary.prompt && <small className="agent-card-prompt" title={exactTextString(primary.prompt)}>{promptPreview(primary.prompt)}</small>}
          {!primary.enabled && <small className="agent-card-disabled">{disabledReasonSentence(primary.disabled_reason)}</small>}
          {results?.[primary.token] && <small className="agent-card-result" role="status" data-testid="agent-card-result">{displayText(resultSentence(results[primary.token]), DISPLAY_LIMITS.detail)}</small>}
          {primary.last_delivery && <small className="agent-card-delivery">{displayText(deliverySentence(primary.last_delivery), DISPLAY_LIMITS.message)}</small>}
        </div>
      )}
    </article>
  );
}
