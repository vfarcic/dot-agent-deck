import { useEffect, useState, type KeyboardEvent } from "react";
import { BellRing, ChevronLeft, ChevronRight, X } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import { actionLabel, cardStatusClass, cardStatusLabel, deliverySentence, formatClockTime, resultSentence, type CardOverlayStep, type CardResults, disabledReasonSentence, fieldValueText, linkOf, multilineText, needsYouSentence, reportStatusLabel, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { AgentCardAction } from "../types";
import { cardRef, ExactText, ExactTextNote, SourceBadge } from "./AgentCard";

/** The members the overlay's controls dispatch through. */
export type CardOverlayControls = Pick<VoiceActionContext, "showAgentCard" | "stepAgentCard" | "closeAgentCard" | "runCardAction" | "replyToAgentCard" | "confirmCardAction">;

/**
 * PRD #2374 — the one card overlay, opened from a card or a table row's Show
 * card. It shows everything on the entry: the deck-checked status and facts,
 * what the agent reported (labelled as the agent's), its questions, and every
 * action with the exact text it sends, its state and its last delivery.
 * Content-sized; full screen at narrow widths (CSS). `[`, `]` and `n` move
 * between cards in the dashboard's order without closing it; `Escape` closes.
 *
 * `card` is `undefined` once the agent it showed is gone: the overlay says so
 * and keeps its navigation, so the next card is one key away.
 */
export function AgentCardOverlay({ card, lastName, position, count, step, results, controls }: {
  card: PlacedCard | undefined;
  /** The name of the card that was open, for the closed notice. */
  lastName: string;
  /** 1-based place of the card in the dashboard's order; 0 when it is gone. */
  position: number;
  count: number;
  step: CardOverlayStep | undefined;
  results: CardResults;
  controls: CardOverlayControls;
}) {
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => { controls.closeAgentCard(); });
  const [reply, setReply] = useState("");
  const cardKey = card ? `${card.deckId}\u0000${card.entry.instance_id}\u0000${card.entry.key}` : undefined;
  useEffect(() => { setReply(""); }, [cardKey]);
  /* A spoken reply opens this step with its words in the box, to be read,
     edited and sent by hand. */
  useEffect(() => { if (step?.kind === "reply" && step.text !== undefined) setReply(step.text); }, [step]);
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    const target = event.target;
    if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true']")) {
      if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); VOICE_ACTIONS.closeAgentCard.run(controls); }
      return;
    }
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); VOICE_ACTIONS.closeAgentCard.run(controls); }
    else if (event.key === "]") { event.preventDefault(); VOICE_ACTIONS.nextAgentCard.run(controls); }
    else if (event.key === "[") { event.preventDefault(); VOICE_ACTIONS.previousAgentCard.run(controls); }
    else if (event.key === "n") { event.preventDefault(); VOICE_ACTIONS.nextNeedsYouCard.run(controls); }
  };
  const entry = card?.entry;
  const name = entry ? displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id) : displayText(lastName, DISPLAY_LIMITS.name);
  const confirming = step?.kind === "confirm" && entry ? entry.actions.find((action) => action.id === step.actionId) : undefined;
  /* The agent changed the action after this step opened: the step shows the
     new text, sends nothing, and asks for a fresh review (review B2). */
  const changed = confirming !== undefined && step?.kind === "confirm" && confirming.token !== step.token;
  const replyAction = entry?.actions.find((action) => action.id === "reply");
  const optionIds = new Set(entry?.questions.flatMap((question) => question.options.map((option) => option.action_id)) ?? []);
  const actions = entry?.actions.filter((action) => !optionIds.has(action.id)) ?? [];
  const pullRequest = entry?.facts.pull_request as { number?: number; title?: string; state?: string; url?: string } | null | undefined;
  return (
    <div className="dialog-backdrop agent-card-backdrop" role="presentation" onMouseDown={() => VOICE_ACTIONS.closeAgentCard.run(controls)}>
      <section
        ref={ref}
        tabIndex={-1}
        className="agent-card-overlay"
        role="dialog"
        aria-modal="true"
        aria-labelledby="agent-card-overlay-title"
        data-testid="agent-card-overlay"
        data-card-key={entry?.key}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        <header className="agent-card-overlay-header">
          <div className="agent-card-overlay-identity">
            <h2 id="agent-card-overlay-title">{name}</h2>
            {entry && <span className={`status-label card-status ${cardStatusClass(entry.status)}`}>{cardStatusLabel(entry.status)}</span>}
            {entry && <SourceBadge source="deck" />}
            {entry?.needs_you && <span className="agent-card-needs-you" data-testid="agent-card-overlay-needs-you"><BellRing size={11} aria-hidden="true" />{needsYouSentence(entry)}</span>}
          </div>
          <nav className="agent-card-overlay-nav" aria-label="Cards">
            <span className="agent-card-overlay-position">{position > 0 ? `${position} of ${count}` : `${count} ${count === 1 ? "card" : "cards"}`}</span>
            <button type="button" className="icon-button" aria-label="Previous card" title="Previous card ([)" data-testid="agent-card-previous" onClick={() => VOICE_ACTIONS.previousAgentCard.run(controls)}><ChevronLeft size={15} /></button>
            <button type="button" className="icon-button" aria-label="Next card" title="Next card (])" data-testid="agent-card-next" onClick={() => VOICE_ACTIONS.nextAgentCard.run(controls)}><ChevronRight size={15} /></button>
            <button type="button" className="button secondary compact" aria-label="Next card that needs you" title="Next card that needs you (n)" data-testid="agent-card-next-needs-you" onClick={() => VOICE_ACTIONS.nextNeedsYouCard.run(controls)}><BellRing size={13} aria-hidden="true" /><span>Next needing you</span></button>
            <button type="button" className="icon-button" aria-label="Close card" title="Close card (Escape)" data-testid="agent-card-close" onClick={() => VOICE_ACTIONS.closeAgentCard.run(controls)}><X size={15} /></button>
          </nav>
        </header>

        {!entry && <p className="agent-card-gone" role="status" data-testid="agent-card-gone">This agent was closed.</p>}

        {entry && card && confirming && (
          <div className="agent-card-confirm" role="alertdialog" aria-labelledby="agent-card-confirm-title" data-testid="agent-card-confirm">
            <h3 id="agent-card-confirm-title">{displayText(actionLabel(entry, confirming), DISPLAY_LIMITS.name)}?</h3>
            {confirming.source === "agent" && <p className="agent-card-hint"><SourceBadge source="agent" /> The agent wrote this action and its text.</p>}
            {confirming.prompt
              ? <><p>This sends {name} exactly:</p><blockquote className="agent-card-prompt-text" data-testid="agent-card-confirm-prompt"><ExactText text={confirming.prompt} /></blockquote><ExactTextNote text={confirming.prompt} /></>
              : <p>{confirming.kind === "close_keep_worktree" ? `This closes ${name} and keeps its worktree.` : confirming.kind === "close" ? `This closes ${name}.` : `This runs ${displayText(confirming.label, DISPLAY_LIMITS.name)} on ${name}.`}</p>}
            {confirming.confirm_detail && <p className="agent-card-confirm-detail">{displayText(confirming.confirm_detail, DISPLAY_LIMITS.detail)}</p>}
            {changed && (
              <p className="agent-card-changed" role="status" data-testid="agent-card-confirm-changed">
                The agent changed this action while you were reviewing it. Nothing was sent; review the text above before sending it.
                {" "}<button type="button" className="button secondary compact" data-testid="agent-card-confirm-review" onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: confirming.id, cardActionReview: true })}>I have reviewed the new text</button>
              </p>
            )}
            <div className="agent-card-confirm-buttons">
              <button type="button" className="button secondary" data-testid="agent-card-confirm-cancel" onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, cardRef(card))}>Cancel</button>
              <button type="button" className={`button ${confirming.kind === "prompt" ? "primary" : "danger"}`} data-testid="agent-card-confirm-send" disabled={!confirming.enabled || changed || results[confirming.token]?.outcome === "pending"} onClick={() => VOICE_ACTIONS.confirmCardAction.run(controls)}>{confirming.kind === "prompt" ? "Send" : displayText(confirming.label, DISPLAY_LIMITS.name)}</button>
            </div>
          </div>
        )}

        {entry && card && (
          <div className="agent-card-overlay-body">
            {entry.report
              ? (
                <section className="agent-card-section" aria-label="What the agent reported">
                  <h3>Report <SourceBadge source="agent" /></h3>
                  <p className="agent-card-report-status">The agent says: <strong>{reportStatusLabel(entry.report.status)}</strong></p>
                  {entry.report.summary && <p className="agent-card-report-summary">{displayText(entry.report.summary, DISPLAY_LIMITS.message)}</p>}
                </section>
              )
              : <p className="agent-card-summary is-empty">No report from this agent yet.</p>}

            {entry.fields.length > 0 && (
              <section className="agent-card-section" aria-label="Fields">
                <h3>Fields <SourceBadge source="agent" /></h3>
                <dl className="agent-card-fields is-detail">
                  {entry.fields.map((field) => {
                    const text = fieldValueText(field.value);
                    const link = linkOf(field.type, field.value);
                    return (
                      <div key={field.name} className="agent-card-field" data-field={field.name}>
                        <dt title={field.description ? displayTitle(field.description) : undefined}>{displayText(field.label, DISPLAY_LIMITS.name)}{field.custom && <span className="card-custom" title="Not in the catalog: the agent added it">custom</span>}</dt>
                        <dd className={field.type === "long_text" ? "is-long" : undefined}>{link ? <a href={link} target="_blank" rel="noreferrer noopener">{displayText(text, DISPLAY_LIMITS.detail)}</a> : field.type === "long_text" ? multilineText(text) : displayText(text, DISPLAY_LIMITS.detail)}</dd>
                      </div>
                    );
                  })}
                </dl>
              </section>
            )}

            {entry.questions.length > 0 && (
              <section className="agent-card-section" aria-label="Questions">
                <h3>Questions <SourceBadge source="agent" /></h3>
                {entry.questions.map((question) => (
                  <div key={question.id} className="agent-card-question" data-question={question.id}>
                    <p className="agent-card-question-text">{multilineText(question.text)}</p>
                    <div className="agent-card-options">
                      {question.options.map((option) => {
                        const action = entry.actions.find((candidate) => candidate.id === option.action_id);
                        const chosen = question.answered?.option_id === option.id;
                        return (
                          <div key={option.id} className="agent-card-action">
                            <button type="button" className={`button ${chosen ? "primary" : "secondary"} compact`} data-testid="agent-card-option" disabled={!action || !action.enabled} title={action && !action.enabled ? `Disabled: ${disabledReasonSentence(action.disabled_reason)}` : undefined} onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: option.action_id, ...(action ? { cardActionToken: action.token } : {}) })}>{displayText(option.label, DISPLAY_LIMITS.name)}{chosen ? " (answered)" : ""}</button>
                            {action && <ActionDetails action={action} result={results[action.token]} />}
                          </div>
                        );
                      })}
                    </div>
                  </div>
                ))}
              </section>
            )}

            <section className="agent-card-section" aria-label="Actions">
              <h3>Actions</h3>
              <div className="agent-card-actions">
                {actions.map((action) => (
                  <div key={action.id} className="agent-card-action" data-action={action.id}>
                    <button
                      type="button"
                      className={`button ${action.kind === "close" || action.kind === "close_keep_worktree" ? "danger" : "secondary"} compact`}
                      data-testid={`agent-card-action-${domIdentity(action.id)}`}
                      disabled={!action.enabled}
                      title={!action.enabled ? `Disabled: ${disabledReasonSentence(action.disabled_reason)}` : undefined}
                      onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: action.id, cardActionToken: action.token })}
                    >{displayText(action.label, DISPLAY_LIMITS.name)}{action.source === "agent" && <span className="card-custom" title="Suggested by the agent, not in the catalog">custom</span>}</button>
                    <ActionDetails action={action} result={results[action.token]} />
                  </div>
                ))}
              </div>
            </section>

            {replyAction && step?.kind === "reply" && (
              <section className="agent-card-section agent-card-reply" aria-label="Reply">
                <h3>Reply to {name}</h3>
                <p className="agent-card-hint">Your words are sent to {name} as they are.</p>
                <textarea
                  aria-label={`Reply to ${name}`}
                  data-testid="agent-card-reply-text"
                  value={reply}
                  rows={3}
                  maxLength={16_000}
                  autoFocus
                  onChange={(event) => setReply(event.target.value)}
                />
                <div className="agent-card-reply-buttons">
                  <button type="button" className="button secondary compact" data-testid="agent-card-reply-cancel" onClick={() => VOICE_ACTIONS.showAgentCard.run(controls, cardRef(card))}>Cancel</button>
                  <button type="button" className="button primary compact" data-testid="agent-card-reply-send" disabled={!replyAction.enabled || reply.trim() === "" || results[replyAction.token]?.outcome === "pending"} onClick={() => VOICE_ACTIONS.replyToAgentCard.run(controls, { ...cardRef(card), cardReplyText: reply, cardReplySend: true })}>Send reply</button>
                </div>
              </section>
            )}

            {(entry.recent_deliveries?.length ?? 0) > 0 && (
              <section className="agent-card-section" aria-label="Sent from this card" data-testid="agent-card-deliveries">
                <h3>Sent from this card <SourceBadge source="deck" /></h3>
                <ol className="agent-card-deliveries">
                  {entry.recent_deliveries!.map((delivery) => (
                    <li key={delivery.message_id} className="agent-card-delivery-record" data-testid="agent-card-delivery-record" data-action={delivery.action_id}>
                      <p><strong>{displayText(delivery.label, DISPLAY_LIMITS.name)}</strong>{" · "}<time dateTime={delivery.at}>{formatClockTime(delivery.at)}</time>{" · "}<span data-testid="agent-card-delivery-state">{displayText(deliverySentence(delivery), DISPLAY_LIMITS.message)}</span></p>
                      <blockquote className="agent-card-prompt-text"><ExactText text={delivery.text} /></blockquote><ExactTextNote text={delivery.text} />
                    </li>
                  ))}
                </ol>
              </section>
            )}

            {pullRequest && (
              <section className="agent-card-section" aria-label="Checked by the deck">
                <h3>Pull request <SourceBadge source="deck" /></h3>
                <p>{pullRequest.number !== undefined ? `#${pullRequest.number}` : ""}{pullRequest.title ? ` ${displayText(pullRequest.title, DISPLAY_LIMITS.name)}` : ""}{pullRequest.state ? ` (${displayText(pullRequest.state, DISPLAY_LIMITS.name)})` : ""}</p>
              </section>
            )}
          </div>
        )}
      </section>
    </div>
  );
}

/** Under an action's button: the exact text it sends, why it is disabled, and what happened to its last press. */
function ActionDetails({ action, result }: { action: AgentCardAction; result: CardResults[string] | undefined }) {
  return (
    <>
      {action.prompt && <><p className="agent-card-prompt-text" data-testid="agent-card-action-prompt"><ExactText text={action.prompt} /></p><ExactTextNote text={action.prompt} /></>}
      {action.text_input && <p className="agent-card-hint">Sends your own words.</p>}
      {action.kind === "open" && <p className="agent-card-hint">Opens the agent's terminal.</p>}
      {action.confirm && action.enabled && <p className="agent-card-hint">Asks you to confirm first.</p>}
      {!action.enabled && <p className="agent-card-disabled">Disabled: {disabledReasonSentence(action.disabled_reason)}</p>}
      {result && <p className="agent-card-result" role="status" data-testid="agent-card-action-result">{displayText(resultSentence(result), DISPLAY_LIMITS.detail)}</p>}
      {action.last_delivery && <p className="agent-card-delivery" data-testid="agent-card-action-delivery">{displayText(deliverySentence(action.last_delivery), DISPLAY_LIMITS.message)}</p>}
    </>
  );
}
