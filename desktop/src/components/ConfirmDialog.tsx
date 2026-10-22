import { useCallback, useState } from "react";
import { CircleStop } from "lucide-react";

import { insideVoicePeer, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";

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
  /* On the overlay stack (issue #1709), so the rest of the window is fenced
     and voice's "close" is Cancel — rather than closing whatever this is drawn
     over and leaving the question on screen. Except where the voice surface
     hosts it (`insideVoicePeer`): known once the backdrop is in the document,
     so the first commit is off the stack and the next one joins it. */
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, onClose);
  return <div ref={placed} className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section ref={ref} tabIndex={-1} className="confirm-dialog" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" onMouseDown={(event) => event.stopPropagation()}><div className="danger-icon"><CircleStop size={20} /></div><h2 id="confirm-title">{state.title}</h2><p>{state.body}</p><div><button className="button secondary" onClick={onClose}>Cancel</button><button className="button danger" disabled={busy} onClick={() => { setBusy(true); void state.action().finally(onClose); }}>{busy ? state.busyLabel : state.label}</button></div></section></div>;
}
