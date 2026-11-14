/**
 * The Add a daemon dialog (PRD #2274) — the add form Settings → Daemons uses,
 * opened over whatever screen the user is on.
 *
 * It is how a user who has never opened Settings finds that the dashboard can
 * show another machine's agents: the Daemon selector's **Add a daemon…** entry,
 * the empty-agents note's **Add a daemon** button and voice's `add_daemon` all
 * open it, through `VOICE_ACTIONS.addDaemon`. The form is the same one Settings
 * renders (`AddDaemonForm.tsx`), so a daemon added here is stored and chosen
 * exactly as one added there is, and appears on the dashboard the same way.
 *
 * Cancel, Escape, a click outside and voice's "close" all close it having
 * stored nothing, and the screen under it is as it was.
 */
import { Check, Server } from "lucide-react";
import { createContext, useEffect, useState } from "react";
import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { DesktopSettingsState } from "../hooks/useDesktopSettings";
import { DaemonAddressFields, DaemonNameRow, useNewDaemon } from "./AddDaemonForm";

/**
 * How a control opens the dialog: the shell's opener, or `undefined` where no
 * shell is mounted (a component rendered on its own), where the controls that
 * open it are not offered. Controls call it through `VOICE_ACTIONS.addDaemon`.
 */
export const AddDaemonOpener = createContext<(() => void) | undefined>(undefined);

export function AddDaemonDialog({ settings, onClose }: { settings: Pick<DesktopSettingsState, "settings" | "save" | "saveError">; onClose: () => void }) {
  /* `keepOnFailedSave`: a save that fails leaves the form on screen with the
     reason under it, rather than closing on a daemon that was not stored. */
  const form = useNewDaemon(settings.settings, settings.save, { startOpen: true, keepOnFailedSave: true });
  const shown = form.draft ?? form.pending;
  const busy = form.confirming || form.pending !== undefined;
  /* On the overlay stack, so the rest of the window is fenced and voice's
     "close" is Cancel (issue #1709). */
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, onClose);
  /* The draft is gone with nothing pending: it was stored, so the dialog's work is done. */
  const done = shown === undefined;
  useEffect(() => {
    if (done) onClose();
  }, [done, onClose]);
  /* The settings save error, once a save from THIS dialog has failed; an
     older one from an unrelated save is not this form's to report. */
  const [saveFailed, setSaveFailed] = useState(false);
  const add = async () => {
    setSaveFailed(false);
    if ((await form.confirm()) === "kept") setSaveFailed(true);
  };

  return (
    <div className="dialog-backdrop" role="presentation" data-testid="add-daemon-backdrop" onMouseDown={onClose}>
      <section
        ref={ref}
        tabIndex={-1}
        className="add-daemon-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="add-daemon-title"
        data-testid="add-daemon-dialog"
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key !== "Escape") return;
          event.stopPropagation();
          onClose();
        }}
      >
        <header>
          <Server size={16} aria-hidden="true" />
          <h2 id="add-daemon-title">Add a daemon</h2>
        </header>
        <p className="add-daemon-lead">
          Show the agents running on another machine on this dashboard. The app reaches the machine over ssh, using your ssh config, and the machine needs <code>dot-agent-deck</code> installed.
        </p>
        {shown && (
          <div className="settings-body add-daemon-fields" data-testid="add-daemon-form">
            {form.naming && <DaemonNameRow id={shown.id} form={form} />}
            <DaemonAddressFields row={shown} onChange={form.edit} disabled={busy} autoFocus />
          </div>
        )}
        {saveFailed && settings.saveError && (
          <p className="settings-error" role="alert" data-testid="add-daemon-error">{settings.saveError}</p>
        )}
        <div className="add-daemon-actions">
          <button className="button secondary" data-testid="add-daemon-cancel" onClick={onClose}>Cancel</button>
          <button className="button primary" data-testid="add-daemon-save" disabled={!form.canConfirm} onClick={() => void add()}>
            <Check size={14} /> {busy ? "Adding…" : "Add this daemon"}
          </button>
        </div>
      </section>
    </div>
  );
}
