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
    void saveSettings(next)
      .then((written) => setSettings(written))
      .catch((cause: unknown) => {
        // Deliberately NOT reverted. The user asked for this and can see it;
        // what failed is making it survive a restart, and saying so is more
        // use than silently undoing a choice they just made.
        setSaveError(cause instanceof Error ? cause.message : String(cause));
      });
  }, [saveSettings]);

  return { settings, path, loaded, saveError, save };
}
