import { useCallback, useEffect, useState, type KeyboardEvent } from "react";
import { ShieldAlert } from "lucide-react";

import { insideVoicePeer, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import {
  APPROVE_AND_START,
  APPROVING,
  publishReviewApprove,
  REVIEW_APPROVING,
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
 * description and prompt, then any role the approval had that the
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
export function ProjectTrustDialog({ review, onApprove, onCancel }: { review: ShownReview; onApprove: () => void; onCancel: () => void }) {
  const { view } = review;
  /* On the overlay stack, so voice's `close` ("cancel", "never mind") is
     Cancel — except where the voice surface hosts it (`ConfirmDialog`'s note). */
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, onCancel);

  /* Voice's approve, published while this review is on screen. */
  useEffect(() => publishReviewApprove(() => {
    if (review.busy) return REVIEW_APPROVING;
    onApprove();
    return undefined;
  }), [review.busy, onApprove]);

  /* The button's own press: never refused, since the button is disabled while an approval is under way. */
  const approveHere = () => {
    onApprove();
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
            {multiLine("description", role.description, "project-trust-description")}
            {multiLine("prompt", role.prompt, "project-trust-prompt")}
          </div>
        ))}
        {view.removedRoles.length > 0 && <div className="trust-field" data-testid="project-trust-removed"><span className="trust-label">Removed since you approved it:</span> <span className="trust-value">{view.removedRoles.join(", ")}</span></div>}
      </div>
      {review.notice && <p className="trust-notice" role="status" data-testid="project-trust-notice">{review.notice}</p>}
      {review.error && <p className="trust-error" role="alert" data-testid="project-trust-error">{review.error}</p>}
      <div>
        <button className="button secondary" onClick={() => VOICE_ACTIONS.cancelProjectCommands.run({ cancelProjectCommands: onCancel })}>Cancel</button>
        <button className="button danger" disabled={review.busy} onClick={() => VOICE_ACTIONS.approveProjectCommands.run({ approveProjectCommands: approveHere })}>{review.busy ? APPROVING : APPROVE_AND_START}</button>
      </div>
    </section>
  </div>;
}
