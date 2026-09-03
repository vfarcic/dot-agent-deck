import { useState } from "react";
import { CircleStop } from "lucide-react";

/**
 * One irreversible-enough act, described in the terms the user needs to decide.
 * Every consumer of this dialog states the RISK in `body` rather than restating
 * the button — Start daemon, Launch live loop, stop-agent and Connect anyway all
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
  return <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section className="confirm-dialog" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" onMouseDown={(event) => event.stopPropagation()}><div className="danger-icon"><CircleStop size={20} /></div><h2 id="confirm-title">{state.title}</h2><p>{state.body}</p><div><button className="button secondary" onClick={onClose}>Cancel</button><button className="button danger" disabled={busy} onClick={() => { setBusy(true); void state.action().finally(onClose); }}>{busy ? state.busyLabel : state.label}</button></div></section></div>;
}
