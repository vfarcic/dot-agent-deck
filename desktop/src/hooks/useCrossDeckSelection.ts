/**
 * The cross-deck round trip (PRD #1105 M6): switch the selected deck when a
 * pane opens on a deck that is not selected, and put the selection back when
 * it closes.
 *
 * # Why there is a switch at all
 *
 * The overview merges every observed deck's agents onto one screen, and a
 * tile's terminal is always the *selected* deck's. So the overview can offer an
 * agent this app cannot currently attach to, and the decision recorded in the
 * PRD is to move the selection rather than to teach the wire to multiplex —
 * cross-deck attach is [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073)
 * and is a `PROTOCOL_VERSION` bump by construction.
 *
 * # What it costs, stated rather than glossed
 *
 * `settings.save` is `desktop_set_settings`, which **persists the document to
 * disk** and then applies it; `retarget_selection` calls `terminal::detach_all`
 * when the deck actually moved, which detaches *every* session. So a cross-deck
 * open costs a full teardown on the way in and another on the way out, plus two
 * persistent writes. The PRD's no-flicker commitment is scoped to the
 * **same-deck** path for exactly this reason, and nothing here should be read
 * as extending it.
 *
 * A transient navigation writing persistent configuration also means the
 * residue after an *unclean* exit is the selection the pane set. That is known,
 * accepted and documented; what is guaranteed is the clean round trip, which
 * restores the document byte for byte.
 *
 * # Why the same-deck path reaches none of that
 *
 * The first thing the effect does is compare the pane's deck against the
 * selected one, so a deck-origin pane — whose agent is always on the selected
 * deck, since the deck surface renders no other — returns before it has read
 * the settings document, let alone written it.
 *
 * # Decided once, when the pane opens
 *
 * The effect keys on the opened deck and on whether the document has loaded,
 * and on nothing else — it is a *transition*, not a continuously reconciled
 * state. Re-running it is harmless (every branch is idempotent: an already
 * correct selection writes nothing), but it deliberately does not chase a
 * selection the user changes underneath an open pane. The revert target is
 * captured at the moment of the switch and never re-captured, so a pane that
 * somehow moved between two non-selected decks still closes back to where it
 * started.
 */
import { useEffect, useRef } from "react";
import { LOCAL_ENDPOINT_SELECTION, endpointSectionToSave, selectionForDeck, selectionToken } from "../lib/endpoints";
import type { DesktopSettingsState } from "./useDesktopSettings";
import type { DeckFleet } from "../types";

/** The stored token in force right now. Absent section means the default. */
function currentToken(settings: DesktopSettingsState): string {
  return settings.settings.endpoints?.selection ?? LOCAL_ENDPOINT_SELECTION;
}

/**
 * Write one selection, through the guard every other write site uses.
 *
 * `endpointSectionToSave` is what stops a client that renders decks from
 * asserting `remote: []` over rows it never saw — the merge-protection
 * obligation `docs/develop/desktop-gui.md` puts on this side. Routing through
 * it also makes a no-op write genuinely no-op rather than merely equal.
 */
function writeSelection(settings: DesktopSettingsState, selection: string): void {
  const section = settings.settings.endpoints;
  const write = endpointSectionToSave(section, { remote: section?.remote ?? [], selection });
  if (!write) return;
  settings.save({ ...settings.settings, endpoints: write });
}

export function useCrossDeckSelection({
  settings,
  fleet,
  selectedDeckId,
  openDeckId,
}: {
  settings: DesktopSettingsState;
  fleet: DeckFleet;
  /** `connection.deckId` of the deck the single-deck surfaces are on. */
  selectedDeckId: string | undefined;
  /** `deckId` of the open agent pane, or `undefined` when none is open. */
  openDeckId: string | undefined;
}): void {
  // Read through a ref so the effect keys on the transition alone. The settings
  // object changes identity on every save — including the save this effect
  // performs — so depending on it would re-enter on our own write.
  const latest = useRef({ settings, fleet, selectedDeckId });
  latest.current = { settings, fleet, selectedDeckId };
  /**
   * The token to put back, captured at the switch. `undefined` means no switch
   * is outstanding, which is also what makes closing a same-deck pane free.
   */
  const revertTo = useRef<string | undefined>(undefined);

  useEffect(() => {
    const { settings, fleet, selectedDeckId } = latest.current;
    if (openDeckId === undefined) {
      const token = revertTo.current;
      if (token === undefined) return;
      revertTo.current = undefined;
      writeSelection(settings, token);
      return;
    }
    // The same-deck path, and the whole of it.
    if (openDeckId === selectedDeckId) return;
    const deck = fleet.find((entry) => entry.connection.deckId === openDeckId);
    if (!deck) return;
    // `undefined` when this document cannot name the deck unambiguously. Not
    // switching leaves the pane without a terminal; switching to a guess would
    // attach a different deck's agent of the same id. See `selectionForDeck`.
    const target = selectionForDeck(settings.settings.endpoints, deck.connection);
    if (!target) return;
    const token = selectionToken(target);
    const current = currentToken(settings);
    if (token === current) return;
    // Never re-captured: the first capture is the selection the user actually
    // chose, and every later one would be a selection this hook set.
    if (revertTo.current === undefined) revertTo.current = current;
    writeSelection(settings, token);
    // `settings.loaded` is a dependency because an agent pane can be the view a
    // tree mounts with, and until the read settles `settings.settings` is the
    // placeholder — which holds no rows, so the switch would silently resolve
    // to nothing. It flips false→true once and every branch above is
    // idempotent, so the retry costs a comparison.
  }, [openDeckId, settings.loaded]);
}
