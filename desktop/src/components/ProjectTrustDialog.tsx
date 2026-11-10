import { useCallback, useLayoutEffect, useState, type KeyboardEvent } from "react";
import { ShieldAlert } from "lucide-react";

import { insideVoicePeer, overlayAbove, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import {
  APPROVAL_IN_FLIGHT_NOTE,
  APPROVE_AND_START,
  APPROVING,
  CLEAR_FALSE,
  CLEAR_LABEL,
  CLEAR_TRUE,
  DECK_LABEL,
  publishReview,
  REVIEW_APPROVING,
  REVIEW_COVERED,
  reviewToken,
  START_FALSE,
  START_LABEL,
  START_TRUE,
  TRUST_INTRO,
  TRUST_TITLE,
  trustStateWords,
  type ShownReview,
} from "../lib/projectTrust";
import { VOICE_ACTIONS } from "../lib/voiceActions";

/**
 * PRD #1885 — `Review project commands`: what starting an orchestration would
 * run, shown before anything runs. The TUI's prompt (`src/trust_prompt.rs`), in
 * the same words: the project, the orchestration and whether it was approved
 * before, then each role's name (marked `changed` or `new`), command, agent,
 * description and prompt, its `start` and `clear` flags (both part of what is
 * approved), then any role the approval had that the
 * orchestration no longer has.
 *
 * **Config text is untrusted.** Every string arrives escaped by the bridge, and
 * every one is rendered as a React text node — never as HTML or Markdown — so a
 * tag, a link or a control character in a project's config is shown as typed.
 *
 * **Enter never approves.** No button takes focus when it opens (the card
 * does), and Enter anywhere inside it is swallowed, as the TUI's prompt ignores
 * it: approving is a click on Approve and start, Space on that button, or
 * voice's "approve project commands". Esc cancels, and so does voice's `close`
 * ("cancel"), through the overlay stack.
 *
 * Both buttons dispatch through the voice action registry (CLAUDE.md rule 23):
 * `approveProjectCommands` is the `approve_project_commands` row, and
 * `cancelProjectCommands` is reached by voice through `close`.
 */
export function ProjectTrustDialog({ review, onApprove, onCancel }: { review: ShownReview; onApprove: (token: string) => void; onCancel: () => void }) {
  const { view } = review;
  /* The review this render shows: an approval from here approves it and
     nothing else (`useProjectTrustReview`'s `approve` compares). */
  const token = reviewToken(review);
  /* On the overlay stack, so voice's `close` ("cancel", "never mind") is
     Cancel — except where the voice surface hosts it (`ConfirmDialog`'s note). */
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, onCancel);

  /* Voice's approve, published with this review's token while it is on
     screen. Voice reads the token when the words are declared and approves
     only while it is still this one (`approveShownReview`), and only while
     nothing is drawn over the dialog.
     A LAYOUT effect, so the token is published in the same commit that puts
     its text on the page: a passive effect runs later, and in between the
     page showed re-shown text while voice still read the old token (and the
     old review's approve). */
  useLayoutEffect(() => publishReview({
    token,
    approve: () => {
      if (overlayAbove(ref.current)) return REVIEW_COVERED;
      if (review.busy) return REVIEW_APPROVING;
      onApprove(token);
      return undefined;
    },
  }), [token, review.busy, onApprove, ref]);

  /* The button's own press: never refused, since the button is disabled while an approval is under way. */
  const approveHere = () => {
    onApprove(token);
    return undefined;
  };

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
    } else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      onCancel();
    }
  };

  const multiLine = (label: string, lines: string[] | undefined, testId: string) => (
    <div className="trust-field">
      <span className="trust-label">{label}:</span>
      {lines === undefined
        ? <span className="trust-value"> (none)</span>
        : <pre className="trust-text" data-testid={testId}>{lines.join("\n")}</pre>}
    </div>
  );

  return <div ref={placed} className="dialog-backdrop" role="presentation">
    <section
      ref={ref}
      tabIndex={-1}
      className="confirm-dialog trust-dialog"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="trust-title"
      data-testid="project-trust-dialog"
      onMouseDown={(event) => event.stopPropagation()}
      onKeyDown={onKeyDown}
    >
      <div className="danger-icon"><ShieldAlert size={20} /></div>
      <h2 id="trust-title">{TRUST_TITLE}</h2>
      <div className="trust-body" tabIndex={0} data-testid="project-trust-body">
        {review.deckName !== undefined && <div className="trust-field"><span className="trust-label">{DECK_LABEL}:</span> <span className="trust-value" data-testid="project-trust-deck">{review.deckName}</span></div>}
        <div className="trust-field"><span className="trust-label">Project:</span> <span className="trust-value" data-testid="project-trust-project">{view.project}</span></div>
        {view.configDir && <div className="trust-field"><span className="trust-label">Config in:</span> <span className="trust-value">{view.configDir}</span></div>}
        <div className="trust-field"><span className="trust-label">Orchestration:</span> <span className="trust-value" data-testid="project-trust-orchestration">{view.orchestration}</span></div>
        <div className="trust-field"><span className="trust-label">State:</span> <strong className="trust-mark" data-testid="project-trust-state">{trustStateWords(view)}</strong></div>
        <p className="trust-intro">{TRUST_INTRO}</p>
        {view.roles.map((role, index) => (
          <div className="trust-role" key={`${index}-${role.name}`} data-testid="project-trust-role">
            <div className="trust-field"><span className="trust-label">Role:</span> <strong className="trust-value">{role.name}</strong>{role.change && <strong className="trust-mark" data-testid="project-trust-role-change"> {role.change}</strong>}</div>
            <div className="trust-field"><span className="trust-label">command:</span> <code className="trust-value" data-testid="project-trust-command">{role.command}</code></div>
            {role.agent !== undefined && <div className="trust-field"><span className="trust-label">agent:</span> <span className="trust-value">{role.agent}</span></div>}
            <div className="trust-field"><span className="trust-label">{START_LABEL}:</span> <span className="trust-value" data-testid="project-trust-start">{role.start ? START_TRUE : START_FALSE}</span></div>
            <div className="trust-field"><span className="trust-label">{CLEAR_LABEL}:</span> <span className="trust-value" data-testid="project-trust-clear">{role.clear ? CLEAR_TRUE : CLEAR_FALSE}</span></div>
            {multiLine("description", role.description, "project-trust-description")}
            {multiLine("prompt", role.prompt, "project-trust-prompt")}
          </div>
        ))}
        {view.removedRoles.length > 0 && <div className="trust-field" data-testid="project-trust-removed"><span className="trust-label">Removed since you approved it:</span> <span className="trust-value">{view.removedRoles.join(", ")}</span></div>}
      </div>
      {review.busy && <p className="trust-notice" role="status" data-testid="project-trust-in-flight">{APPROVAL_IN_FLIGHT_NOTE}</p>}
      {review.notice && <p className="trust-notice" role="status" data-testid="project-trust-notice">{review.notice}</p>}
      {review.error && <p className="trust-error" role="alert" data-testid="project-trust-error">{review.error}</p>}
      <div>
        <button className="button secondary" onClick={() => VOICE_ACTIONS.cancelProjectCommands.run({ cancelProjectCommands: onCancel })}>Cancel</button>
        <button className="button danger" disabled={review.busy} onClick={() => VOICE_ACTIONS.approveProjectCommands.run({ approveProjectCommands: approveHere })}>{review.busy ? APPROVING : APPROVE_AND_START}</button>
      </div>
    </section>
  </div>;
}
