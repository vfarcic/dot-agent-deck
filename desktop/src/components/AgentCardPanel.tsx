import { useEffect, useRef } from "react";
import { BellRing } from "lucide-react";

import { cardStatusClass, cardStatusLabel, deliverySentence, fieldValueText, formatClockTime, multilineText, needsYouSentence, reportStatusLabel, resultSentence, disabledReasonSentence, type CardResults, type PlacedCard } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText, displayTitle } from "../lib/displayText";
import { agentTypeText, issueLabels, referenceDetail, referenceView } from "../lib/agentCardsTree";
import { VOICE_ACTIONS } from "../lib/voiceActions";
import { ENDED_ROLE_WORDS } from "../lib/endedRoles";
import type { PaneCardControls } from "../lib/paneCards";
import type { AgentCardCheckedRef, CardTabSection, EndedRole } from "../types";
import { CardActionIconRow, CardReference, cardRef, ExactText, ExactTextNote, IssueLabelChip, panelIconActions, SourceBadge, type ReferenceControls } from "./AgentCard";

/**
 * PR #2491 decision 2026-11-26 — what the agent pane's Card tab shows (the
 * tab's panel; it was a panel beside the terminal from 2026-11-25, and the
 * card overlay before that). It shows, in two labelled parts, what the deck
 * checked (the status, the pull requests and issues the agent reported as the
 * deck checked them, what was sent from the card) and what the agent says
 * (its summary and its own status, why it needs you, its fields, and its
 * questions with their answer buttons), then the agent's actions as the same
 * grouped icon row a card draws, pressed the same way. The deck's own actions
 * are not repeated: Open is the pane itself, a reply is typed into the
 * terminal, and Close is the header's Close agent.
 *
 * It is drawn over the terminal's box, which stays mounted under it, so
 * switching tabs never changes the terminal's size.
 */
export function AgentCardPanel({ card, name, results, tree, ended, notice, section, controls, readOnly = false, cardActions = false }: {
  /** The agent's card; `undefined` when the dashboard lists none for it (any more). */
  card: PlacedCard | undefined;
  /** What the pane calls the agent, for a panel with no card. */
  name: string;
  results: CardResults;
  /** Whether the card's daemon builds the tree, so its checked references are its own (issue #2418). */
  tree: boolean;
  /** Issue #1965 — the exited orchestration role this card is for: the panel says why and shows its last output in full. */
  ended?: EndedRole;
  /** Why the last thing pressed did not happen. */
  notice?: string;
  /** The part of the Card tab the pane was opened at: a card's references `+N` opens it at the references, its actions `+N` at the actions. */
  section?: CardTabSection;
  controls: PaneCardControls;
  /**
   * PR #2491 audit M1 — the pane of an agent that has ended: the panel shows
   * the card but offers none of its actions and none of its answers, however
   * the card marks them, and says so.
   */
  readOnly?: boolean;
  /**
   * PR #2491 (the maintainer, 2026-11-25) — whether the card's daemon offers
   * its Card actions editor, so the Actions heading has an Edit beside it
   * (never in a read-only panel).
   */
  cardActions?: boolean;
}) {
  const entry = card?.entry;
  const shownName = entry ? displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id) : displayText(name, DISPLAY_LIMITS.name);
  /* Opened from a card's `+N`: the part it counted (the references, or the
     actions) is scrolled into view and takes the keyboard, once per
     navigation that asked for it, so the next Tab continues into it and a
     screen reader lands there (PR #2491 review J6). `preventScroll` keeps the
     scroll just made; a card update re-renders with the same section and key,
     so it never takes focus. */
  const refsSection = useRef<HTMLElement>(null);
  const actionsSection = useRef<HTMLElement>(null);
  const cardKey = card ? `${card.deckId}\u0000${card.entry.key}` : undefined;
  const checkedRefs = tree ? entry?.checked_refs ?? [] : [];
  const icons = entry && !readOnly ? panelIconActions(entry) : [];
  const hasSection = section === "references" ? checkedRefs.length > 0 : section === "actions" ? icons.length > 0 : false;
  const handedOff = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (section === undefined) handedOff.current = undefined;
    if (section === undefined || !hasSection || cardKey === undefined || handedOff.current === `${section}\u0000${cardKey}`) return;
    const node = (section === "references" ? refsSection : actionsSection).current;
    if (!node) return;
    handedOff.current = `${section}\u0000${cardKey}`;
    node.scrollIntoView?.({ block: "start" });
    node.focus({ preventScroll: true });
  }, [section, cardKey, hasSection]);
  const pullRequest = entry?.facts.pull_request as { number?: number; title?: string; state?: string } | null | undefined;
  /* PR #2491 (the maintainer, 2026-11-25) — the Edit beside Actions: the card's daemon's editor, as its ⋯ menu opens it. */
  const editActions = cardActions && !readOnly && controls.openCardActionsEditor
    ? { openCardActionsEditor: controls.openCardActionsEditor, reportRefused: controls.reportRefused }
    : undefined;
  const offersSomething = entry !== undefined && (panelIconActions(entry).length > 0 || entry.questions.some((question) => question.options.length > 0));
  /* What became of each press from this card, in the order the actions are listed. */
  const answered = entry ? entry.actions.flatMap((action) => (results[action.token] ? [{ action, result: results[action.token]! }] : [])) : [];
  return (
    <div className="agent-card-panel" aria-label={`${shownName}'s card`} data-testid="agent-card-panel" data-card-key={entry?.key}>
      {notice && <p className="agent-card-panel-notice" role="status" data-testid="agent-card-panel-notice">{displayText(notice, DISPLAY_LIMITS.message)}</p>}
      {/* PR #2491 review J2: the card is gone or its daemon is not answering; the panel keeps its place and offers none of its actions. */}
      {!entry && <p className="agent-card-gone" role="status" data-testid="agent-card-gone">This card is not available right now.</p>}
      {entry && card && (
        <>
          <section className="agent-card-section agent-card-panel-part" aria-label="Checked by the deck" data-testid="agent-card-panel-deck">
            <h3>Status <SourceBadge source="deck" /></h3>
            <p className="agent-card-panel-status">
              <span className={`status-label card-status ${cardStatusClass(entry.status)}`} data-testid="agent-card-panel-status">{cardStatusLabel(entry.status)}</span>
              {agentTypeText(entry.agent_type) && <span className="agent-card-agent-type" data-testid="agent-card-panel-agent-type">{displayText(agentTypeText(entry.agent_type)!, DISPLAY_LIMITS.name)}</span>}
            </p>
            {ended && (
              <div className="agent-card-ended-full" aria-label="Why this role exited" data-testid="agent-card-panel-ended" title={displayTitle(ended.headline)}>
                <p className="agent-card-ended-reason"><strong>{ENDED_ROLE_WORDS.status}:</strong> {displayText(ended.reason, DISPLAY_LIMITS.title)}</p>
                {ended.lastOutput.length > 0 && (
                  <>
                    <span className="agent-card-ended-label">{ENDED_ROLE_WORDS.lastOutput}:</span>
                    <pre>{ended.lastOutput.map((line) => displayText(line, DISPLAY_LIMITS.title)).join("\n")}</pre>
                  </>
                )}
              </div>
            )}
            {pullRequest && (
              <p className="agent-card-panel-pr" data-testid="agent-card-panel-branch-pr">Pull request {pullRequest.number !== undefined ? `#${pullRequest.number}` : ""}{pullRequest.title ? ` ${displayText(pullRequest.title, DISPLAY_LIMITS.name)}` : ""}{pullRequest.state ? ` (${displayText(pullRequest.state, DISPLAY_LIMITS.name)})` : ""}</p>
            )}
            {checkedRefs.length > 0 && (
              <section ref={refsSection} tabIndex={-1} className="agent-card-panel-refs" aria-label="Pull requests and issues" data-testid="agent-card-panel-refs">
                <h4>Pull requests and issues</h4>
                <ul className="agent-card-panel-ref-list">
                  {checkedRefs.map((reference, index) => <PanelReference key={`${reference.field}\u0000${index}`} card={card} reference={reference} controls={controls} />)}
                </ul>
              </section>
            )}
            {(entry.recent_deliveries?.length ?? 0) > 0 && (
              <section className="agent-card-panel-deliveries" aria-label="Sent from this card" data-testid="agent-card-deliveries">
                <h4>Sent from this card</h4>
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
          </section>

          <section className="agent-card-section agent-card-panel-part" aria-label="What the agent says" data-testid="agent-card-panel-agent">
            <h3>Report <SourceBadge source="agent" /></h3>
            {entry.report
              ? (
                <>
                  <p className="agent-card-report-status">The agent says: <strong>{reportStatusLabel(entry.report.status)}</strong></p>
                  {entry.report.summary && <p className="agent-card-report-summary">{displayText(entry.report.summary, DISPLAY_LIMITS.message)}</p>}
                </>
              )
              : <p className="agent-card-summary is-empty">No report from this agent yet.</p>}
            {entry.needs_you && <p className="agent-card-needs-you agent-card-panel-needs-you" data-testid="agent-card-panel-needs-you"><BellRing size={12} aria-hidden="true" />{needsYouSentence(entry)}</p>}
            {entry.fields.length > 0 && (
              <dl className="agent-card-fields is-detail" aria-label="Fields">
                {entry.fields.map((field) => {
                  /* PR #2491 audit K3: a field is what the agent says, so a
                     web address in it is text to select and copy, never a
                     link; the pull requests and issues the deck checked
                     itself open from their badges above. */
                  const text = fieldValueText(field.value);
                  return (
                    <div key={field.name} className="agent-card-field" data-field={field.name}>
                      <dt title={field.description ? displayTitle(field.description) : undefined}>{displayText(field.label, DISPLAY_LIMITS.name)}{field.custom && <span className="card-custom" title="Not in the catalog: the agent added it">custom</span>}</dt>
                      <dd className={field.type === "long_text" ? "is-long" : undefined}>{field.type === "long_text" ? multilineText(text) : displayText(text, DISPLAY_LIMITS.detail)}</dd>
                    </div>
                  );
                })}
              </dl>
            )}
            {entry.questions.length > 0 && (
              <div className="agent-card-panel-questions" aria-label="Questions">
                {entry.questions.map((question) => (
                  <div key={question.id} className="agent-card-question" data-question={question.id}>
                    <p className="agent-card-question-text">{multilineText(question.text)}</p>
                    {!readOnly && <div className="agent-card-options">
                      {question.options.map((option) => {
                        const action = entry.actions.find((candidate) => candidate.id === option.action_id);
                        const chosen = question.answered?.option_id === option.id;
                        return (
                          <button
                            key={option.id}
                            type="button"
                            className={`button ${chosen ? "primary" : "secondary"} compact`}
                            data-testid="agent-card-option"
                            disabled={!action || !action.enabled}
                            title={action && !action.enabled ? `Disabled: ${disabledReasonSentence(action.disabled_reason)}` : undefined}
                            onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: option.action_id, ...(action ? { cardActionToken: action.token } : {}) })}
                          >{displayText(option.label, DISPLAY_LIMITS.name)}{chosen ? " (answered)" : ""}</button>
                        );
                      })}
                    </div>}
                  </div>
                ))}
              </div>
            )}
          </section>

          {readOnly && offersSomething && <p className="agent-card-hint agent-card-panel-read-only" role="note" data-testid="agent-card-panel-read-only">{`${shownName} has ended, so its actions and answers are not available.`}</p>}
          {icons.length > 0 && (
            <section ref={actionsSection} tabIndex={-1} className="agent-card-section agent-card-panel-part agent-card-panel-actions" aria-label="Actions" data-testid="agent-card-panel-actions">
              <div className="agent-card-panel-actions-heading">
                <h3>Actions</h3>
                {editActions && (
                  <button
                    type="button"
                    className="agent-card-panel-edit-actions"
                    data-testid="agent-card-panel-edit-actions"
                    aria-label="Edit card actions"
                    title="Edit card actions"
                    onClick={() => VOICE_ACTIONS.openCardActionsEditor.run(editActions, card.deckId)}
                  >Edit</button>
                )}
              </div>
              <CardActionIconRow card={card} actions={icons} controls={controls} label={`${shownName}'s actions`} />
            </section>
          )}
          {answered.length > 0 && (
            <ul className="agent-card-panel-results" aria-label="What became of your presses">
              {answered.map(({ action, result }) => (
                <li key={action.token} className="agent-card-result" role="status" data-testid="agent-card-action-result" data-action={action.id}>
                  <strong>{displayText(action.label, DISPLAY_LIMITS.name)}</strong>: {displayText(resultSentence(result), DISPLAY_LIMITS.detail)}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </div>
  );
}

/**
 * Issue #2418 review (D-R1) — one reported reference in the panel's list: its
 * badge, which opens its page as the card's does, what the deck's check found
 * and how fresh it is (or that it is still to be checked, or why it cannot
 * be), and what the agent wrote, labelled as the agent's.
 */
function PanelReference({ card, reference, controls }: { card: PlacedCard; reference: AgentCardCheckedRef; controls: ReferenceControls }) {
  const view = referenceView(reference);
  const detail = referenceDetail(reference);
  const labels = issueLabels(reference);
  return (
    <li className={`agent-card-panel-ref is-${view.status}`} data-testid="agent-card-panel-ref" data-field={reference.field} data-kind={reference.kind} data-ref-status={view.status}>
      <div className="agent-card-panel-ref-head">
        <CardReference card={card} reference={reference} controls={controls} scope="panel" />
        {detail.checked && <span className="agent-card-panel-ref-checked" data-testid="agent-card-panel-ref-checked">{displayText(detail.checked, DISPLAY_LIMITS.detail)}</span>}
      </div>
      {detail.title && <p className="agent-card-panel-ref-title">{displayText(detail.title, DISPLAY_LIMITS.message)}</p>}
      {labels.length > 0 && (
        <p className="agent-card-panel-labels" data-testid="agent-card-panel-issue-labels" aria-label={`Labels of issue ${reference.number !== undefined ? `#${reference.number}` : displayText(reference.claim, DISPLAY_LIMITS.name)}`}>
          {labels.map((label, index) => <IssueLabelChip key={`${label.name}\u0000${index}`} label={label} testId="agent-card-panel-issue-label" />)}
        </p>
      )}
      <p className={`agent-card-panel-ref-note${view.status === "stale" ? " is-stale" : ""}`} data-testid="agent-card-panel-ref-note">{displayText(detail.note, DISPLAY_LIMITS.message)}</p>
      <p className="agent-card-panel-ref-claim" data-testid="agent-card-panel-ref-claim"><SourceBadge source="agent" /> {reference.kind === "issue" ? "Issue" : "Pull request"} as reported: {displayText(detail.claim, DISPLAY_LIMITS.detail)}</p>
    </li>
  );
}
