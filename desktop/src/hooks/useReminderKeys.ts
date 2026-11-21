import { useEffect, useRef, useState, type RefObject } from "react";

import { insideVoicePeer, overlayAbove } from "./useInertBackground";

/**
 * PRD #211 — the keyboard of a reminder that opens on its own: the
 * Antigravity notice and the star reminder (PRD #2174). While the dialog is
 * open it owns the keyboard: keys reach neither the page behind it nor the
 * app's window shortcuts (with the exceptions below), and a key makes one of
 * its `keys` choices only once the dialog has been on screen for `armMs`, so
 * a key the user was already typing elsewhere answers nothing.
 *
 * **One capture-phase listener on `window`, registered when the dialog
 * mounts.** The app's window shortcuts (`?`, the digits, `Ctrl+K`, `Escape`'s
 * blanket dismissal) listen there in the bubble phase, so a handler on the
 * dialog's own element reached them only when focus was inside the dialog
 * and the key was one of its choices; with focus on `<body>` (a click on the
 * backdrop) none of it applied. Stopping each key at the window's capture
 * phase covers both, and the 400 ms before the keys arm as well. What it does
 * not stop are the window capture listeners registered before it, such as
 * the zoom keys (`useZoom`), which change the app's scale rather than act
 * behind the dialog.
 *
 * Stopping propagation leaves each key's default action alone, so `Tab` still
 * moves focus among the dialog's buttons (the background is inert), and
 * `Enter` or `Space` on a focused button still presses it — once the keys are
 * armed. Before that, and for a held key's repeats, those two are
 * default-prevented, and {@link ReminderKeys.pressed} refuses the click a
 * keyboard activation makes (`detail` 0) while a mouse click still chooses.
 *
 * Left alone: a key whose target is in the voice surface (its numbered
 * choice dialog handles its own keys), and every key while another overlay
 * is drawn over the dialog, which owns them then.
 */
export type ReminderKeys = {
  /** Whether the keys choose yet. */
  ready: boolean;
  /** Whether a button's `click` may choose: always once armed, and before that only from a pointer. */
  pressed: (event: { detail: number }) => boolean;
};

export function useReminderKeys<C>(ref: RefObject<HTMLElement | null>, keys: Readonly<Record<string, C>>, armMs: number, onChoose: (choice: C) => void): ReminderKeys {
  const [ready, setReady] = useState(false);
  const armed = useRef(false);
  const choose = useRef(onChoose);
  choose.current = onChoose;

  useEffect(() => {
    const timer = window.setTimeout(() => {
      armed.current = true;
      setReady(true);
    }, armMs);
    return () => window.clearTimeout(timer);
  }, [armMs]);

  useEffect(() => {
    /** Whether this dialog owns `event`, and so stops it here. */
    const owns = (event: KeyboardEvent): boolean => {
      const node = ref.current;
      if (!node || overlayAbove(node)) return false;
      const target = event.target;
      return !(target instanceof Element && !node.contains(target) && insideVoicePeer(target));
    };
    const activates = (event: KeyboardEvent) => event.key === "Enter" || event.key === " ";
    const onKeyDown = (event: KeyboardEvent) => {
      if (!owns(event)) return;
      event.stopImmediatePropagation();
      if (!armed.current || event.repeat) {
        if (activates(event) || Object.hasOwn(keys, event.key)) event.preventDefault();
        return;
      }
      if (event.ctrlKey || event.metaKey || event.altKey || !Object.hasOwn(keys, event.key)) return;
      event.preventDefault();
      choose.current(keys[event.key]);
    };
    /* A `Space` presses a button on its release, so the release is held back too. */
    const onKeyUp = (event: KeyboardEvent) => {
      if (!owns(event)) return;
      event.stopImmediatePropagation();
      if (!armed.current && activates(event)) event.preventDefault();
    };
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
    };
  }, [keys, ref]);

  return { ready, pressed: (event) => armed.current || event.detail > 0 };
}
