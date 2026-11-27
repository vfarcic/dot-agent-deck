import type { KeyboardEvent } from "react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { CardReviewView } from "../hooks/useAgentCardsView";
import { actionLabel, resultSentence } from "../lib/agentCards";
import { DISPLAY_LIMITS, displayIdentity, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { cardRef, ExactText, ExactTextNote, SourceBadge } from "./AgentCard";

/**
 * PRD #2374, PR #2491 decision 2026-11-25 — the single-action review: a small
 * dialog showing exactly what one card action does before anything is sent.
 * It opens for an action that asks first, both closes, one whose text the
 * agent wrote, and any press from a control that showed only the action's
 * label or could not show its text whole — from a card's icon, the Card
 * panel's, the table's row menu or voice. Only its Send sends, and only the
 * version of the action it shows: an action the agent changed while it was
 * open sends nothing until the new text is reviewed (review B2). After Send
 * it says what became of the press, and closes on Close.
 */
export function AgentCardReviewDialog({ review, controls }: {
  review: CardReviewView;
  controls: Pick<VoiceActionContext, "confirmCardAction" | "closeCardReview" | "runCardAction"> & Partial<Pick<VoiceActionContext, "reportRefused">>;
}) {
  /* Voice's `close` reaches this through the overlay stack, as `Escape` does. */
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => { controls.closeCardReview(); });
  const { card, results } = review;
  const entry = card?.entry;
  const name = entry ? displayIdentity(entry.name, DISPLAY_LIMITS.name, entry.pane_id) : displayText(review.lastName, DISPLAY_LIMITS.name);
  /* Once sent, the review shows the action it sent and what became of it,
     not the card's current state: a Close removes the card and an agent can
     stop offering an action it got, and neither unsends it (review J4). */
  const sent = review.sent ? review.reviewed : undefined;
  const action = sent?.action ?? entry?.actions.find((candidate) => candidate.id === review.actionId);
  /* The agent changed the action after the review opened: it shows the new
     text, sends nothing, and asks for a fresh review (review B2). */
  const changed = !sent && action !== undefined && action.token !== review.token;
  const result = sent?.result ?? results[review.token];
  const title = sent ? sent.label : entry && action ? actionLabel(entry, action) : "";
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "Escape" || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    event.stopPropagation();
    VOICE_ACTIONS.closeCardReview.run(controls);
  };
  return (
    <div className="dialog-backdrop agent-card-review-backdrop" role="presentation" onMouseDown={() => VOICE_ACTIONS.closeCardReview.run(controls)}>
      <section
        ref={ref}
        tabIndex={-1}
        className="agent-card-confirm"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="agent-card-confirm-title"
        data-testid="agent-card-confirm"
        data-sent={review.sent ? "true" : undefined}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={onKeyDown}
      >
        {!action || (!sent && (!entry || !card))
          ? (
            <>
              <h3 id="agent-card-confirm-title">{name}</h3>
              <p className="agent-card-gone" role="status" data-testid="agent-card-gone">{entry ? "The agent no longer offers this action. Nothing was sent." : "This agent was closed. Nothing was sent."}</p>
            </>
          )
          : (
            <>
              <h3 id="agent-card-confirm-title">{displayText(title, DISPLAY_LIMITS.name)}?</h3>
              {action.source === "agent" && <p className="agent-card-hint"><SourceBadge source="agent" /> The agent wrote this action and its text.</p>}
              {action.prompt
                ? <><p>This sends {name} exactly:</p><blockquote className="agent-card-prompt-text" data-testid="agent-card-confirm-prompt"><ExactText text={action.prompt} /></blockquote><ExactTextNote text={action.prompt} /></>
                : <p>{action.kind === "close_keep_worktree" ? `This closes ${name} and keeps its worktree.` : action.kind === "close" ? `This closes ${name}.` : `This runs ${displayText(action.label, DISPLAY_LIMITS.name)} on ${name}.`}</p>}
              {action.confirm_detail && <p className="agent-card-confirm-detail">{displayText(action.confirm_detail, DISPLAY_LIMITS.detail)}</p>}
              {changed && card && (
                <p className="agent-card-changed" role="status" data-testid="agent-card-confirm-changed">
                  The agent changed this action while you were reviewing it. Nothing was sent; review the text above before sending it.
                  {" "}<button type="button" className="button secondary compact" data-testid="agent-card-confirm-review" onClick={() => VOICE_ACTIONS.runCardAction.run(controls, { ...cardRef(card), cardActionId: action.id, cardActionReview: true })}>I have reviewed the new text</button>
                </p>
              )}
              {review.sent && result && <p className="agent-card-result" role="status" data-testid="agent-card-action-result">{displayText(resultSentence(result), DISPLAY_LIMITS.detail)}</p>}
            </>
          )}
        <div className="agent-card-confirm-buttons">
          {review.sent || !entry || !action || !card
            ? <button type="button" className="button primary" data-testid="agent-card-confirm-close" onClick={() => VOICE_ACTIONS.closeCardReview.run(controls)}>Close</button>
            : (
              <>
                <button type="button" className="button secondary" data-testid="agent-card-confirm-cancel" onClick={() => VOICE_ACTIONS.closeCardReview.run(controls)}>Cancel</button>
                <button type="button" className={`button ${action.kind === "prompt" ? "primary" : "danger"}`} data-testid="agent-card-confirm-send" disabled={!action.enabled || changed || result?.outcome === "pending"} onClick={() => VOICE_ACTIONS.confirmCardAction.run(controls)}>{action.kind === "prompt" ? "Send" : displayText(action.label, DISPLAY_LIMITS.name)}</button>
              </>
            )}
        </div>
      </section>
    </div>
  );
}
