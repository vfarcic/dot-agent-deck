import { createContext, useContext } from "react";

/**
 * PR #1451, round 3 — whether voice is on, for the surfaces that render
 * differently while it is: a list that numbers its items, a listing that pages
 * instead of scrolling.
 *
 * The voice panel owns the toggle and reports it (`onVoiceChange`); the shell
 * holds that report in state and provides it here, the same shape as the
 * dictation mark. It is a mirror for rendering only — nothing reads it to
 * decide what a voice command does, and nothing writes it but the panel's
 * report. Outside a provider it reads `false`, which is how every list looks
 * with voice off.
 */
export const VoiceOn = createContext(false);

/** Whether voice is on, as the voice panel last reported it. */
export function useVoiceOn(): boolean {
  return useContext(VoiceOn);
}
