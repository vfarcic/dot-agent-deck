import { useCallback, useEffect, useMemo, useState } from "react";
import type { DeckOverlay } from "../lib/voiceActions";

/** The two screens the rail sits beside. An agent pane is drawn OVER one of them. */
export type RailScreen = "deck" | "overview";

/**
 * Everything the rail can show as open over a screen: the deck's overlays, and
 * the shortcut sheet its bottom button opens. The sheet is not a
 * {@link DeckOverlay} because no registry entry opens it.
 */
export type ShellOverlay = DeckOverlay | "shortcuts";

export type ShellOverlayState = Partial<Record<ShellOverlay, boolean>>;

/** One screen's overlays, and their setter already scoped to that screen. */
export type ScreenOverlays = {
  open: ShellOverlayState;
  set: (overlay: ShellOverlay, open: boolean) => void;
};

const NONE: ShellOverlayState = {};

/**
 * Issue #1197 — the overlay booleans, held where the ONE rail is rendered.
 *
 * They used to be `useState` inside the deck, which is fine for panels only the
 * deck mounts but not for the rail that shows them as active: the rail is now
 * rendered once, beside whichever screen is up, and Settings opens over the
 * overview too. So the state lives above both screens, tagged with the screen
 * it was opened on.
 *
 * **The tag is what keeps leaving a screen equivalent to what unmounting it
 * used to be.** Before, navigating away unmounted the deck and every overlay
 * with it, so coming back found them closed. Here the state survives the
 * navigation, so it is read only while its tag matches the screen on show —
 * masked in the same render, so the other screen never sees an overlay it does
 * not own — and the effect below then clears the stale tag, so returning does
 * not resurrect what was open before. Opening an overlay on ANOTHER screen (a
 * rail entry for a deck panel, pressed on the overview) writes that screen's
 * tag in the same batch as the navigation, so the effect finds it current and
 * leaves it alone.
 */
export function useShellOverlays(screen: RailScreen) {
  const [state, setState] = useState<{ screen: RailScreen; open: ShellOverlayState }>({ screen, open: NONE });
  const open = state.screen === screen ? state.open : NONE;

  useEffect(() => {
    setState((current) => (current.screen === screen ? current : { screen, open: NONE }));
  }, [screen]);

  /** Stable, so a listener that closed over the first render's copy keeps working. */
  const setOverlay = useCallback((on: RailScreen, overlay: ShellOverlay, value: boolean) => {
    setState((current) => ({ screen: on, open: { ...(current.screen === on ? current.open : NONE), [overlay]: value } }));
  }, []);
  const closeOverlays = useCallback((on: RailScreen) => setState({ screen: on, open: NONE }), []);
  const setDeckOverlay = useCallback((overlay: ShellOverlay, value: boolean) => setOverlay("deck", overlay, value), [setOverlay]);
  const deck = useMemo<ScreenOverlays>(() => ({ open: screen === "deck" ? open : NONE, set: setDeckOverlay }), [open, screen, setDeckOverlay]);

  return { open, setOverlay, closeOverlays, deck };
}
