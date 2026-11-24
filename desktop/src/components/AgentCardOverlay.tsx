import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { BellRing, ChevronLeft, ChevronRight, X } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import { actionLabel, cardStatusClass, cardStatusLabel, deliverySentence, formatClockTime, resultSentence, type CardOverlayStep, type CardResults, disabledReasonSentence, fieldValueText, linkOf, multilineText, needsYouSentence, reportStatusLabel, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle, domIdentity } from "../lib/displayText";
import { agentTypeText, issueLabels, referenceDetail, referenceView } from "../lib/agentCardsTree";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { ENDED_ROLE_WORDS } from "../lib/endedRoles";
import type { AgentCardAction, AgentCardCheckedRef, EndedRole } from "../types";
import { batchChoiceHover, CardReference, cardRef, ExactText, ExactTextNote, IssueLabelChip, SourceBadge, type ReferenceControls } from "./AgentCard";
import { BatchChoiceIcon, batchIconName } from "./AgentCardsGrid";
import { ActionTipButton } from "./ActionTipButton";

/** The members the overlay's controls dispatch through. */
export type CardOverlayControls = Pick<VoiceActionContext, "showAgentCard" | "stepAgentCard" | "closeAgentCard" | "runCardAction" | "replyToAgentCard" | "confirmCardAction" | "openCardReference"> & Partial<Pick<VoiceActionContext, "reportRefused" | "runCardActionBatch">>;

/**
 * PRD #2374 — the one card overlay, opened from a card or a table row's Show
 * card. It shows everything on the entry: the deck-checked status and facts,
 * what the agent reported (labelled as the agent's), every pull request and
 * issue it reported as the deck checked them (issue #2418 review: a card's
 * `+N` opens the overlay there), its questions, and every action with the
 * exact text it sends, its state and its last delivery. It leads with what a
 * reply needs (issue #2418, D14): the agent's summary, its open questions with
 * the options it offered, and the reply field, shown whenever the card offers
 * a reply; references, fields, actions and deliveries follow.
 * Content-sized; full screen at narrow widths (CSS). `[`, `]` and `n` move
 * between cards in the dashboard's order without closing it; `Escape` closes.
 *
 * `card` is `undefined` once the agent it showed is gone: the overlay says so
 * and keeps its navigation, so the next card is one key away.
 */
export function AgentCardOverlay({ card, lastName, position, count, step, results, tree, notice, controls, ended, batch }: {
  card: PlacedCard | undefined;
  /** The name of the card that was open, for the closed notice. */
  lastName: string;
  /** 1-based place of the card in the dashboard's order; 0 when it is gone. */
  position: number;
  count: number;
  step: CardOverlayStep | undefined;
  results: CardResults;
  /** Whether the card's daemon builds the tree, so its checked references are its own. */
  tree: boolean;
  /** Why the last thing pressed did not happen (issue #2418 review, F2): said here, since the overlay covers the dashboard's banner. */
  notice?: string;
  controls: CardOverlayControls;
  /** Issue #1965 — the exited orchestration role this card is for: the overlay says why and shows its last output in full. */
  ended?: EndedRole;
  /**
   * Issue #2399 — the actions this card can send to itself and every card
   * under it (`under` of them), when its daemon does batches and it has cards
   * under it. Each opens the batch's confirmation; nothing is sent from here.
   */
  batch?: { choices: readonly { id: string; label: string; icon?: string | null; kind?: string; text?: string; differs?: boolean }[]; under: number };
}) {
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => { controls.closeAgentCard(); });
  const [reply, setReply] = useState("");
  const cardKey = card ? `${card.deckId}\u0000${card.entry.instance_id}\u0000${card.entry.key}` : undefined;
  /* Once the daemon has sent a reply from the box, the box empties, so a
     second press cannot send the same words again (issue #2418, Qodo); a
     reply that failed or was refused stays in the box to be retried, and
     words typed while it was sending stay. The words are recorded when Send
     reply sends them, with the card and the action token they went to and
     the answer that token held before, so only the answer to that send can
     empty the box. Leaving the card forgets them: the box starts empty on
     every card, and an answer seen again on returning is not a new send
     (issue #2418 review, merge P2). */
  const sendingReply = useRef<{ cardKey: string; token: string; text: string; before: CardResults[string] | undefined } | undefined>(undefined);
  useEffect(() => { setReply(""); sendingReply.current = undefined; }, [cardKey]);
  useEffect(() => {
    const sending = sendingReply.current;
    if (sending === undefined || sending.cardKey !== cardKey) return;
    const answer = results[sending.token];
    if (answer === undefined || answer === sending.before || answer.outcome === "pending") return;
    sendingReply.current = undefined;
    if (answer.outcome === "sent") setReply((current) => (current === sending.text ? "" : current));
  }, [results, cardKey]);
  /* The Send reply button's controls: the overlay's own, with the reply
     recorded once the deck has taken it to send. */
  const replyControls: CardOverlayControls = {
    ...controls,
    replyToAgentCard: (target, text, sendNow) => {
      const token = card?.entry.actions.find((action) => action.id === "reply")?.token;
      const before = token === undefined ? undefined : results[token];
      const refused = controls.replyToAgentCard(target, text, sendNow);
      if (refused === undefined && sendNow === true && cardKey !== undefined && token !== undefined) sendingReply.current = { cardKey, token, text, before };
      return refused;
    },
  };
  /* A spoken reply opens this step with its words in the box, to be read,
     edited and sent by hand. */
  useEffect(() => { if (step?.kind === "reply" && step.text !== undefined) setReply(step.text); }, [step]);
  /* The reply field is always shown when the card offers a reply (issue
     #2418, D14); pressing Reply, or a spoken reply, puts the focus in it. */
  const replyBox = useRef<HTMLTextAreaElement>(null);
  const atReply = step?.kind === "reply";
  useEffect(() => { if (atReply) replyBox.current?.focus(); }, [atReply, cardKey]);
  /* Opened from a card's `+N`: the references are scrolled into view and take
     the focus, so the keyboard is on them. */
  const refsSection = useRef<HTMLElement>(null);
  const atReferences = step?.kind === "references";
  useEffect(() => {
    if (!atReferences) return;
    const section = refsSection.current;
    if (!section) return;
    section.scrollIntoView?.({ block: "start" });
    section.focus({ preventScroll: true });
  }, [atReferences, cardKey]);
  /* A press that opens the review step (issue #2418 round 2, R2-A1) may come
     from a button far down the overlay: the step, drawn at the top, is
     scrolled into view and takes the focus, so it is what the user sees. */
  const confirmBox = useRef<HTMLDivElement>(null);
  const confirmKey = step?.kind === "confirm" ? `${step.actionId}\u0000${step.token}` : undefined;
  useEffect(() => {
    if (confirmKey === undefined) return;
    const box = confirmBox.current;
    if (!box) return;
    box.scrollIntoView?.({ block: "start" });
    box.focus({ preventScroll: true });
  }, [confirmKey, cardKey]);
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
  const checkedRefs = tree ? entry?.checked_refs ?? [] : [];
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
            {entry && agentTypeText(entry.agent_type) && <span className="agent-card-agent-type" data-testid="agent-card-overlay-agent-type">{displayText(agentTypeText(entry.agent_type)!, DISPLAY_LIMITS.name)}</span>}
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

        {notice && <p className="agent-card-overlay-notice" role="status" data-testid="agent-card-overlay-notice">{displayText(notice, DISPLAY_LIMITS.message)}</p>}

        {!entry && <p className="agent-card-gone" role="status" data-testid="agent-card-gone">This agent was closed.</p>}

        {entry && card && confirming && (
          <div ref={confirmBox} tabIndex={-1} className="agent-card-confirm" role="alertdialog" aria-labelledby="agent-card-confirm-title" data-testid="agent-card-confirm">
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
            {ended && (
              <section className="agent-card-section agent-card-ended-full" aria-label="Why this role exited" data-testid="agent-card-overlay-ended" title={displayTitle(ended.headline)}>
                <h3>{ENDED_ROLE_WORDS.status} <SourceBadge source="deck" /></h3>
                <p className="agent-card-ended-reason">{displayText(ended.reason, DISPLAY_LIMITS.title)}</p>
                {ended.lastOutput.length > 0 && (
                  <>
                    <span className="agent-card-ended-label">{ENDED_ROLE_WORDS.lastOutput}:</span>
                    <pre>{ended.lastOutput.map((line) => displayText(line, DISPLAY_LIMITS.title)).join("\n")}</pre>
                  </>
                )}
              </section>
            )}
            {entry.report
              ? (
                <section className="agent-card-section" aria-label="What the agent reported">
                  <h3>Report <SourceBadge source="agent" /></h3>
                  <p className="agent-card-report-status">The agent says: <strong>{reportStatusLabel(entry.report.status)}</strong></p>
                  {entry.report.summary && <p className="agent-card-report-summary">{displayText(entry.report.summary, DISPLAY_LIMITS.message)}</p>}
                </section>
              )
              : <p className="agent-card-summary is-empty">No report from this agent yet.</p>}

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

            {replyAction && (
              <section className="agent-card-section agent-card-reply" aria-label="Reply">
                <h3>Reply to {name}</h3>
                <p className="agent-card-hint">Your words are sent to {name} as they are.</p>
                <textarea
                  ref={replyBox}
                  aria-label={`Reply to ${name}`}
                  data-testid="agent-card-reply-text"
                  value={reply}
                  rows={3}
                  maxLength={16_000}
                  onChange={(event) => setReply(event.target.value)}
                />
                <div className="agent-card-reply-buttons">
                  <button type="button" className="button primary compact" data-testid="agent-card-reply-send" disabled={!replyAction.enabled || reply.trim() === "" || results[replyAction.token]?.outcome === "pending"} onClick={() => VOICE_ACTIONS.replyToAgentCard.run(replyControls, { ...cardRef(card), cardReplyText: reply, cardReplySend: true })}>Send reply</button>
                </div>
              </section>
            )}

            {checkedRefs.length > 0 && (
              <section ref={refsSection} tabIndex={-1} className="agent-card-section agent-card-overlay-refs" aria-label="Pull requests and issues" data-testid="agent-card-overlay-refs">
                <h3>Pull requests and issues <SourceBadge source="deck" /></h3>
                <ul className="agent-card-overlay-ref-list">
                  {checkedRefs.map((reference, index) => <OverlayReference key={`${reference.field}\u0000${index}`} card={card} reference={reference} controls={controls} />)}
                </ul>
              </section>
            )}

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

            {batch && controls.runCardActionBatch && batch.choices.length > 0 && (
              <section className="agent-card-section agent-card-batch" aria-label={`Send to ${name} and every agent under it`} data-testid="agent-card-batch">
                <h3>{name} and the {batch.under} {batch.under === 1 ? "agent" : "agents"} under it</h3>
                <p className="agent-card-hint">Sends one action to all of them. You see who gets what, and who is skipped and why, before anything is sent.</p>
                <div className="agent-card-actions">
                  {batch.choices.map((choice) => (
                    <ActionTipButton
                      key={choice.id}
                      className="icon-button agent-cards-batch-icon"
                      data-testid={`agent-card-batch-action-${domIdentity(choice.id)}`}
                      data-icon={batchIconName(choice)}
                      aria-label={displayText(choice.label, DISPLAY_LIMITS.name)}
                      tip={batchChoiceHover(choice, `${name} and every agent under it`)}
                      onClick={() => VOICE_ACTIONS.runCardActionBatch.run(controls as Pick<VoiceActionContext, "runCardActionBatch">, { ...cardRef(card), cardActionId: choice.id })}
                    ><BatchChoiceIcon choice={choice} /></ActionTipButton>
                  ))}
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
      {action.prompt && <><blockquote className="agent-card-prompt-text" data-testid="agent-card-action-prompt"><ExactText text={action.prompt} /></blockquote><ExactTextNote text={action.prompt} /></>}
      {action.text_input && <p className="agent-card-hint">Sends your own words.</p>}
      {action.kind === "open" && <p className="agent-card-hint">Opens the agent's terminal.</p>}
      {action.confirm && action.enabled && <p className="agent-card-hint">Asks you to confirm first.</p>}
      {!action.enabled && <p className="agent-card-disabled">Disabled: {disabledReasonSentence(action.disabled_reason)}</p>}
      {result && <p className="agent-card-result" role="status" data-testid="agent-card-action-result">{displayText(resultSentence(result), DISPLAY_LIMITS.detail)}</p>}
      {action.last_delivery && <p className="agent-card-delivery" data-testid="agent-card-action-delivery">{displayText(deliverySentence(action.last_delivery), DISPLAY_LIMITS.message)}</p>}
    </>
  );
}

/**
 * Issue #2418 review (D-R1) — one reported reference in the overlay's list:
 * its badge, which opens its page as the card's does, what the deck's check
 * found and how fresh it is (or that it is still to be checked, or why it
 * cannot be), and what the agent wrote, labelled as the agent's.
 */
function OverlayReference({ card, reference, controls }: { card: PlacedCard; reference: AgentCardCheckedRef; controls: ReferenceControls }) {
  const view = referenceView(reference);
  const detail = referenceDetail(reference);
  const labels = issueLabels(reference);
  return (
    <li className={`agent-card-overlay-ref is-${view.status}`} data-testid="agent-card-overlay-ref" data-field={reference.field} data-kind={reference.kind} data-ref-status={view.status}>
      <div className="agent-card-overlay-ref-head">
        <CardReference card={card} reference={reference} controls={controls} scope="overlay" />
        {detail.checked && <span className="agent-card-overlay-ref-checked" data-testid="agent-card-overlay-ref-checked">{displayText(detail.checked, DISPLAY_LIMITS.detail)}</span>}
      </div>
      {detail.title && <p className="agent-card-overlay-ref-title">{displayText(detail.title, DISPLAY_LIMITS.message)}</p>}
      {labels.length > 0 && (
        <p className="agent-card-overlay-labels" data-testid="agent-card-overlay-issue-labels" aria-label={`Labels of issue ${reference.number !== undefined ? `#${reference.number}` : displayText(reference.claim, DISPLAY_LIMITS.name)}`}>
          {labels.map((label, index) => <IssueLabelChip key={`${label.name}\u0000${index}`} label={label} testId="agent-card-overlay-issue-label" />)}
        </p>
      )}
      <p className={`agent-card-overlay-ref-note${view.status === "stale" ? " is-stale" : ""}`} data-testid="agent-card-overlay-ref-note">{displayText(detail.note, DISPLAY_LIMITS.message)}</p>
      <p className="agent-card-overlay-ref-claim" data-testid="agent-card-overlay-ref-claim"><SourceBadge source="agent" /> {reference.kind === "issue" ? "Issue" : "Pull request"} as reported: {displayText(detail.claim, DISPLAY_LIMITS.detail)}</p>
    </li>
  );
}
