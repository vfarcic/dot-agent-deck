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
 * stored nothing, and the screen under it is as it was — including while the
 * name is being checked, which they cancel. Once the save itself has started
 * it is the user's add, so the dialog stays until it settles.
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

/** What voice's "close" answers while the add is being saved. */
export const ADD_DAEMON_SAVING = "The daemon is being added; the dialog closes once it is saved.";

export function AddDaemonDialog({ settings, onClose }: { settings: Pick<DesktopSettingsState, "settings" | "save" | "saveError">; onClose: () => void }) {
  const form = useNewDaemon(settings.settings, settings.save, { startOpen: true });
  const shown = form.draft ?? form.pending;
  const busy = form.confirming || form.pending !== undefined;
  /* Every way out goes through here: it cancels a confirmation still checking
     the name, so nothing is stored after the user has left, and it refuses
     while the save is running, which cannot be called back. */
  const dismiss = (): string | undefined => {
    if (form.pending) return ADD_DAEMON_SAVING;
    form.cancel();
    onClose();
    return undefined;
  };
  /* On the overlay stack, so the rest of the window is fenced and voice's
     "close" is Cancel (issue #1709). */
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, dismiss);
  /* The draft is gone with nothing pending: it was stored, so the dialog's work is done. */
  const done = shown === undefined;
  useEffect(() => {
    if (done) onClose();
  }, [done, onClose]);
  /* The settings save error, once a save from THIS dialog came back without
     the daemon in the file; an older error from an unrelated save, or a
     refused name, is not a save failure to report here. */
  const [saveFailed, setSaveFailed] = useState(false);
  const add = async () => {
    setSaveFailed(false);
    if ((await form.confirm()) === "restored") setSaveFailed(true);
  };

  return (
    <div className="dialog-backdrop" role="presentation" data-testid="add-daemon-backdrop" onMouseDown={() => void dismiss()}>
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
          dismiss();
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
          <button className="button secondary" data-testid="add-daemon-cancel" disabled={form.pending !== undefined} onClick={() => void dismiss()}>Cancel</button>
          <button className="button primary" data-testid="add-daemon-save" disabled={!form.canConfirm} onClick={() => void add()}>
            <Check size={14} /> {busy ? "Adding…" : "Add this daemon"}
          </button>
        </div>
      </section>
    </div>
  );
}
