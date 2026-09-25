/**
 * Issue #1247 — what the New agent dialog keeps when it is closed without
 * being discarded, so reopening it puts the form back.
 *
 * **It is a record of CHOICES, never a copy of the dialog's state.** The deck's
 * listing, its new-agent options and the directory's orchestrations are live
 * data with their own sequence counters, and restoring them from a copy would
 * show a listing, a Mode row or an agent registry the deck may no longer have.
 * So the draft holds only what the user chose or typed, and the dialog replays
 * the choices against fresh answers on reopen, dropping any the answers no
 * longer offer (see `NewAgentDialog`'s restore).
 *
 * **Which actions keep it and which discard it:**
 *
 * - KEPT by every close: the header's X, Esc, a backdrop click, the browser's
 *   `q`, and voice's `close` (including its `cancel` and `never mind`, which
 *   close rather than discard — the safer reading of a word a steered model
 *   could pick).
 * - DISCARDED by the dialog's Discard button and voice's `discard_new_agent`
 *   (a whole-utterance row, since it cannot be taken back); by a start the
 *   deck accepted, whether or not the agent was listed in time; and by a close
 *   during the "waiting for the deck to list it" phase, because that form has
 *   already been started.
 * - LOST, not discarded, when the agent overview unmounts — leaving it for the
 *   deck screen — because the draft lives in the overview's state. The dialog
 *   is modal, so that takes a deliberate `open_deck`, which is held to the
 *   whole utterance while the dialog is open.
 */
export interface NewAgentDraft {
  /** The wire `deckId` the form was on, when a deck was chosen. */
  deckId?: string;
  /** What that deck was called, for the sentence that says it is gone. */
  deckName?: string;
  /** The directory the browser was showing — a path the deck returned. */
  browsing?: string;
  /** The directory chosen for the agent, as the deck returned it. */
  directory?: { path: string; displayPath: string };
  /** The Mode chip in force (`"none"` for a plain agent). */
  mode: string;
  /** The Name field, and whether a person (or a spoken "name it") set it. */
  name: string;
  nameTouched: boolean;
  /** The Command field, and whether it was edited (typed, or an agent chosen by voice). */
  command: string;
  commandTouched: boolean;
}

/** The Mode id of a plain agent — the dialog's `NO_MODE`. */
export const NO_MODE_ID = "none";

/**
 * Whether a draft holds anything a reopen would put back that a fresh form
 * would not: a deck, or an edited field. An untouched Name or Command is
 * derived from the deck and the directory, so on its own it is nothing.
 */
export function draftWorthKeeping(draft: NewAgentDraft): boolean {
  return draft.deckId !== undefined || draft.nameTouched || draft.commandTouched;
}

/**
 * Whether restoring `draft` puts back something the user would otherwise have
 * to redo — the cue for the dialog's "restored" notice. A deck and a browsing
 * position alone are silent: with one eligible deck that is exactly what a
 * fresh open shows, and a notice on every reopen would be noise.
 */
export function draftHasEdits(draft: NewAgentDraft): boolean {
  return draft.directory !== undefined || draft.nameTouched || draft.commandTouched || draft.mode !== NO_MODE_ID;
}
