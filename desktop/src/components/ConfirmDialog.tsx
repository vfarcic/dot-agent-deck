import { useCallback, useEffect, useRef, useState } from "react";
import { CircleStop } from "lucide-react";

import { insideVoicePeer, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import { sanitizeText } from "../lib/displayText";
import type { WorktreeCloseReport } from "../lib/worktreeClose";
import { KeptWorktreeText } from "./KeptWorktreeText";

/** What a close button passes its action: whether the "Keep the worktree" box was ticked (issue #2436). */
export interface ConfirmChoice {
  keepWorktree: boolean;
}

/** A confirmation's act. Most ignore the choice; a close that can keep its worktree reads it. */
export type ConfirmAction = (choice: ConfirmChoice) => Promise<void>;

/**
 * One irreversible-enough act, described in the terms the user needs to decide.
 * Every consumer of this dialog states the RISK in `body` rather than restating
 * the button — Start daemon, Activate orchestration, stop-agent and Connect anyway all
 * do, and a body that only repeats the label is the sign a confirmation is
 * ceremony rather than a decision.
 */
export interface ConfirmState {
  title: string;
  body: string;
  label: string;
  busyLabel: string;
  action: ConfirmAction;
  worktreePreview?: () => Promise<WorktreeCloseReport>;
  /**
   * Issue #1958, drawn as a checkbox since issue #2436: the close can keep the
   * dispatched worktree. The dialog shows "Keep the worktree" under the
   * worktree preview only when `worktreePreview` names a worktree to keep,
   * which it does only for a deck that can keep one, so an older deck never
   * offers it. Every close button then runs its action with the box's answer.
   */
  keepWorktree?: boolean;
  /**
   * Issue #2436: a second close beside `label`'s — an orchestration role's
   * dialog offers closing the whole orchestration — drawn after it, so the
   * dialog has three buttons at most. `primary` marks it as the choice the
   * dialog leads with (filled); otherwise `label`'s close is.
   */
  alternative?: { label: string; action: ConfirmAction; primary?: boolean };
  /**
   * PRD #2119: a question that changes nothing irreversible — Generate config
   * asks an agent to write a file the user can review — is drawn without the
   * stop icon and the danger colour, so it does not read as a stop. Absent on
   * every destructive confirmation, which keep their look and words.
   */
  neutral?: boolean;
  /** The dismissing button's word; "Cancel" when absent. */
  cancelLabel?: string;
}

/**
 * Lives here rather than in `App.tsx` because the deck is no longer the only
 * screen that asks for a confirmation: the overview offers Connect anyway on an
 * incompatible daemon (issue #801) and renders *instead of* the deck, so it
 * cannot reach into it. Behaviour is unchanged — same markup, same
 * `alertdialog` role, same busy latch.
 */
export function ConfirmDialog({ state, onClose }: { state: ConfirmState; onClose: () => void }) {
  const [busy, setBusy] = useState<"confirm" | "alternative">();
  const [preview, setPreview] = useState<{ state: ConfirmState; report: WorktreeCloseReport | null }>();
  /* The box's answer, for the confirmation it was given on: a newer one starts unticked. */
  const [keepChecked, setKeepChecked] = useState<{ state: ConfirmState; checked: boolean }>();
  const report = preview?.state === state ? preview.report : null;
  const kept = report?.kept ?? null;
  const keepPath = state.keepWorktree ? report?.keepWorktree ?? null : null;
  const keepWorktree = keepPath !== null && keepChecked?.state === state && keepChecked.checked;
  const checking = Boolean(state.worktreePreview && preview?.state !== state);
  useEffect(() => {
    if (!state.worktreePreview) return;
    let active = true;
    // Best-effort like the TUI: the backend bounds the request, and an error
    // leaves the original confirmation available rather than blocking a close.
    void state.worktreePreview().catch(() => null).then((report) => {
      if (active) setPreview({ state, report });
    });
    return () => { active = false; };
  }, [state]);
  /* On the overlay stack (issue #1709), so the rest of the window is fenced
     and voice's "close" is Cancel — rather than closing whatever this is drawn
     over and leaving the question on screen. Except where the voice surface
     hosts it (`insideVoicePeer`): known once the backdrop is in the document,
     so the first commit is off the stack and the next one joins it. */
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, onClose);
  /* The confirmation on screen now, or `undefined` once this dialog is gone.
     An action settles after Cancel, a click outside or voice's "close" may
     already have dismissed its confirmation, and a newer one may be up by
     then: its completion closes only the confirmation it ran for (Qodo, PR #1804). */
  const shown = useRef<ConfirmState | undefined>(state);
  shown.current = state;
  useEffect(() => {
    shown.current = state;
    return () => { shown.current = undefined; };
  }, [state]);
  const run = (which: "confirm" | "alternative", action: ConfirmAction) => {
    setBusy(which);
    const asked = state;
    void action({ keepWorktree }).finally(() => { if (shown.current === asked) onClose(); });
  };
  const confirm = () => run("confirm", state.action);
  const alternative = state.alternative;
  /* Issue #2436: with two closes, the one the dialog leads with is filled and the other outlined. */
  const leads = (which: "confirm" | "alternative") => alternative !== undefined && (which === "alternative") === Boolean(alternative.primary);
  const tone = state.neutral ? "primary" : "danger";
  return <div ref={placed} className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section ref={ref} tabIndex={-1} className={`confirm-dialog confirm-question${kept ? " has-worktree-preview" : ""}`} role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" onMouseDown={(event) => event.stopPropagation()}>{!state.neutral && <div className="danger-icon"><CircleStop size={20} /></div>}<h2 id="confirm-title">{state.title}</h2><p>{state.body}</p>{kept && <p className="worktree-close-preview"><KeptWorktreeText kept={kept} /></p>}{keepPath && <label className="keep-worktree-option" title={`Keeps ${sanitizeText(keepPath)}`}><input type="checkbox" checked={keepWorktree} disabled={Boolean(busy)} onChange={(event) => setKeepChecked({ state, checked: event.target.checked })} /> Keep the worktree</label>}<div><button className="button secondary" onClick={onClose}>{state.cancelLabel ?? "Cancel"}</button><button className={`button ${tone}${leads("confirm") ? " leads" : ""}`} disabled={Boolean(busy) || checking} onClick={confirm}>{busy === "confirm" ? state.busyLabel : checking ? "Checking worktree…" : state.label}</button>{alternative && <button className={`button ${tone}${leads("alternative") ? " leads" : ""}`} disabled={Boolean(busy) || checking} onClick={() => run("alternative", alternative.action)}>{busy === "alternative" ? state.busyLabel : alternative.label}</button>}</div></section></div>;
}
