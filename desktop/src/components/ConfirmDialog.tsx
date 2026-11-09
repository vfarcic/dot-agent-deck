import { useCallback, useEffect, useRef, useState } from "react";
import { CircleStop } from "lucide-react";

import { insideVoicePeer, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { KeptWorktree } from "../lib/worktreeClose";
import { KeptWorktreeText } from "./KeptWorktreeText";

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
  action: () => Promise<void>;
  worktreePreview?: () => Promise<KeptWorktree | null>;
}

/**
 * Lives here rather than in `App.tsx` because the deck is no longer the only
 * screen that asks for a confirmation: the overview offers Connect anyway on an
 * incompatible daemon (issue #801) and renders *instead of* the deck, so it
 * cannot reach into it. Behaviour is unchanged — same markup, same
 * `alertdialog` role, same busy latch.
 */
export function ConfirmDialog({ state, onClose }: { state: ConfirmState; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [preview, setPreview] = useState<{ state: ConfirmState; kept: KeptWorktree | null }>();
  const kept = preview?.state === state ? preview.kept : null;
  const checking = Boolean(state.worktreePreview && preview?.state !== state);
  useEffect(() => {
    if (!state.worktreePreview) return;
    let active = true;
    // Best-effort like the TUI: the backend bounds the request, and an error
    // leaves the original confirmation available rather than blocking a close.
    void state.worktreePreview().catch(() => null).then((kept) => {
      if (active) setPreview({ state, kept });
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
  const confirm = () => {
    setBusy(true);
    const asked = state;
    void asked.action().finally(() => { if (shown.current === asked) onClose(); });
  };
  return <div ref={placed} className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section ref={ref} tabIndex={-1} className={`confirm-dialog${kept ? " has-worktree-preview" : ""}`} role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" onMouseDown={(event) => event.stopPropagation()}><div className="danger-icon"><CircleStop size={20} /></div><h2 id="confirm-title">{state.title}</h2><p>{state.body}</p>{kept && <p className="worktree-close-preview"><KeptWorktreeText kept={kept} /></p>}<div><button className="button secondary" onClick={onClose}>Cancel</button><button className="button danger" disabled={busy || checking} onClick={confirm}>{busy ? state.busyLabel : checking ? "Checking worktree…" : state.label}</button></div></section></div>;
}
