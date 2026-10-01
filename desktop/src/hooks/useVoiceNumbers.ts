import { createContext, useContext, useLayoutEffect } from "react";
import { useVoiceOn } from "./useVoiceOn";
import type { VoiceNumberedEntryDto } from "../lib/voiceNumbers";

/**
 * PR #1451 round 3, change 3 — where a surface declares what it numbers.
 *
 * Two layers, because one screen at a time is numbered: `dialog` is the New
 * agent dialog while it is open, and `screen` the dashboard or the Daemons
 * screen under it. The shell reads the dialog's while there is one, the
 * screen's otherwise (`DeckShell`'s `readNumbered`), and hands that to the
 * voice panel, which answers a spoken number against it.
 *
 * A surface declares its items whether or not voice is on — the numbers it
 * RENDERS follow `useVoiceOn` — so the list the user is looking at is already
 * declared at the moment voice turns on. `undefined` declares nothing: a
 * screen whose rows are behind an open pane or dialog.
 */
export type NumberedLayer = "screen" | "dialog";

export interface VoiceNumbering {
  publish: (layer: NumberedLayer, entries: readonly VoiceNumberedEntryDto[] | undefined) => void;
}

export const VoiceNumberingContext = createContext<VoiceNumbering | undefined>(undefined);

/**
 * Declare `entries` as `layer`'s numbered list for as long as the caller is
 * mounted, updated on every commit that changes it. Layout effects, so the
 * declaration has moved by the time anything reads the screen after the
 * commit; the cleanup withdraws it before a replacing surface declares its own.
 */
export function useNumberedList(layer: NumberedLayer, entries: readonly VoiceNumberedEntryDto[] | undefined): void {
  const numbering = useContext(VoiceNumberingContext);
  useLayoutEffect(() => {
    numbering?.publish(layer, entries);
  }, [entries, layer, numbering]);
  useLayoutEffect(() => () => numbering?.publish(layer, undefined), [layer, numbering]);
}

/**
 * Whether the voice panel's numbered choice is open. While it is, the lists
 * behind it show no numbers: the choice's own "1.", "2." are the numbers on
 * screen then, and a number names one item.
 */
export const VoiceChoiceOpen = createContext(false);

/**
 * Whether the dialog layer declares a numbered list — the New agent dialog, or
 * the open Daemon selector (PR #1451 round 3, change 4). The screen under it
 * shows no numbers then, since a number names one item on screen.
 */
export const DialogNumbered = createContext(false);

/**
 * Whether lists render their numbers: voice is on, no numbered choice is open,
 * and — for the `screen` layer — no dialog over it numbers its own.
 */
export function useNumbersShown(layer: NumberedLayer = "dialog"): boolean {
  const covered = useContext(DialogNumbered) && layer === "screen";
  return useVoiceOn() && !useContext(VoiceChoiceOpen) && !covered;
}
