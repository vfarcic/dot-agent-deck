/**
 * The desktop app's own settings, loaded once at startup and written through
 * the bridge (PRD #803).
 *
 * Deliberately knows nothing about what any setting *means* — it holds the
 * document, the path and the last write error, and nothing else. What the
 * appearance choice does is `lib/appearance.ts`'s, and #741's and #802's
 * sections will add fields here without this file changing at all.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";

export interface DesktopSettingsState {
  settings: DesktopSettingsDto;
  /**
   * Where the document lives. Absent until the load resolves, in the browser
   * preview (which has no filesystem), and if the read failed.
   */
  path?: string;
  /** False until the first load resolves, so the UI can avoid claiming a value it has not read. */
  loaded: boolean;
  /** Set when the last save failed. The change stays applied for this session. */
  saveError?: string;
  save: (next: DesktopSettingsDto) => void;
}

export function useDesktopSettings(runtime: DeckRuntimeState): DesktopSettingsState {
  const { getSettings, saveSettings } = runtime;
  const [settings, setSettings] = useState<DesktopSettingsDto>(DEFAULT_DESKTOP_SETTINGS);
  const [path, setPath] = useState<string>();
  const [loaded, setLoaded] = useState(false);
  const [saveError, setSaveError] = useState<string>();
  // Whether the user has already changed something. The initial load is async,
  // so without this a choice made before it resolves is silently overwritten by
  // the document that was on disk when the app started — the change appears to
  // take, then reverts a moment later.
  const edited = useRef(false);
  // Saves run one at a time, and only the newest one's outcome is applied.
  //
  // Without this, two rapid choices both went out at once: they could reach the
  // disk in either order, and whichever *response* arrived last replaced React
  // state — so a stale document could win twice over. Today the only
  // consequence is a stale appearance choice; it matters more once the document
  // holds a daemon endpoint (#741) or a backend selection (#802). The
  // cross-process half of the same problem — two app windows racing on one
  // field — is #828, and is not something a hook can fix.
  const queue = useRef<Promise<void>>(Promise.resolve());
  const newest = useRef(0);

  useEffect(() => {
    let cancelled = false;
    void getSettings()
      .then((snapshot) => {
        if (cancelled) return;
        // The path is always worth taking; the document is not, if the user has
        // already moved on from it.
        setPath(snapshot.path);
        if (!edited.current) setSettings(snapshot.settings);
      })
      // The live bridge already falls back to defaults, so a rejection here is
      // a bridge that has no settings at all. Defaults keep the app usable.
      .catch(() => undefined)
      .finally(() => { if (!cancelled) setLoaded(true); });
    return () => { cancelled = true; };
  }, [getSettings]);

  const save = useCallback((next: DesktopSettingsDto) => {
    // Applied first, written behind it. PRD #743 requires the appearance change
    // to be visible with no restart, and waiting for a disk write to repaint
    // would put a round trip between the click and the theme.
    edited.current = true;
    setSettings(next);
    setSaveError(undefined);

    const ticket = newest.current + 1;
    newest.current = ticket;
    // Chained rather than fired: the next write starts only once this one has
    // settled, so the last choice made is the last one on disk. The inner
    // handlers never reject, so one failed save cannot break the chain for
    // every save after it.
    queue.current = queue.current.then(() => saveSettings(next)
      .then((written) => {
        // A superseded response is dropped rather than applied — it is an
        // older document, and the user has already moved past it.
        if (newest.current === ticket) setSettings(written);
      })
      .catch((cause: unknown) => {
        // Deliberately NOT reverted. The user asked for this and can see it;
        // what failed is making it survive a restart, and saying so is more
        // use than silently undoing a choice they just made.
        //
        // Superseded failures are dropped for the same reason as superseded
        // successes: the message would be about a choice no longer on screen.
        if (newest.current !== ticket) return;
        setSaveError(cause instanceof Error ? cause.message : String(cause));
      }));
  }, [saveSettings]);

  return { settings, path, loaded, saveError, save };
}
