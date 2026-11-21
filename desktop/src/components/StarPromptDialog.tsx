import { useCallback, useLayoutEffect, useState } from "react";
import { Star } from "lucide-react";

import { insideVoicePeer, overlayAbove, OVERLAY_LAYER, useInertBackground, useOverlayStackEmpty } from "../hooks/useInertBackground";
import { useReminderKeys } from "../hooks/useReminderKeys";
import {
  DONT_ASK_AGAIN,
  LATER,
  publishStarPrompt,
  STAR,
  STAR_BODY_FIRST,
  STAR_BODY_SECOND,
  STAR_KEYS_ARM_MS,
  STAR_PROMPT_COVERED,
  STAR_REPO_DISPLAY,
  STAR_TITLE,
  type StarChoice,
} from "../lib/starPrompt";
import { VOICE_ACTIONS } from "../lib/voiceActions";

/**
 * PRD #2174 — whether the reminder is up: once it is `due` and `ready` (the
 * shell's list of what it waits behind) and no overlay is open at all, read
 * from the overlay stack so whatever the user is in — an agent's pane, the
 * palette, a review — is never covered (Qodo, PR #2321). Once up it stays up
 * until answered, since it is then on the stack itself. A component of its
 * own so that only it, not the whole shell, re-renders as overlays come and go.
 */
export function StarPromptGate({ due, ready, onChoose }: { due: boolean; ready: boolean; onChoose: (choice: StarChoice) => void }) {
  const nothingOpen = useOverlayStackEmpty();
  const [up, setUp] = useState(false);
  const mayOpen = due && ready && nothingOpen;
  if (mayOpen && !up) setUp(true);
  if (!due && up) setUp(false);
  return due && (up || mayOpen) ? <StarPromptDialog onChoose={onChoose} /> : null;
}

/** The key that makes each choice, the TUI prompt's. */
const KEYS: Record<string, StarChoice> = { s: "star", l: "later", Escape: "later", d: "dismiss" };

/**
 * PRD #2174 — the star-repo reminder: the TUI's star prompt, in its words and
 * with its keys. `s` (Star) opens the repository page and stops the reminder,
 * `l` or `Esc` (Later) asks again after another ten launches, and `d` (Don't
 * ask again) stops it.
 *
 * On the overlay stack, so the rest of the window is fenced while it is open,
 * focus goes back to where it was when it closes, and voice's `close`
 * ("cancel", "never mind") is Later, as `Esc` is. It owns the keyboard
 * ({@link useReminderKeys}), as the Antigravity notice does. Each
 * button dispatches through the voice action registry (CLAUDE.md rule 23):
 * `starRepo`, `remindStarLater` and `dismissStarPrompt` are the
 * `star_repo`, `remind_star_later` and `dismiss_star_prompt` rows, which
 * choose only while this dialog is on screen and nothing is drawn over it.
 */
export function StarPromptDialog({ onChoose }: { onChoose: (choice: StarChoice) => void }) {
  const later = useCallback(() => onChoose("later"), [onChoose]);
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, later);

  /* Voice's three choices, published while the dialog is on screen. A LAYOUT
     effect, so they are published in the commit that draws the dialog and
     taken back in the one that removes it. */
  useLayoutEffect(() => publishStarPrompt({
    choose: (choice) => {
      if (overlayAbove(ref.current)) return STAR_PROMPT_COVERED;
      onChoose(choice);
      return undefined;
    },
  }), [onChoose, ref]);

  /* The keys choose only once it has been on screen for `STAR_KEYS_ARM_MS`:
     it opens on its own, so a key already being typed elsewhere must not
     answer it (Qodo, PR #2321). A held key's repeats never choose. Focus is
     moved inside by the overlay stack, which first notes where it was so that
     closing gives it back (PRD #211). */
  const keys = useReminderKeys(ref, KEYS, STAR_KEYS_ARM_MS, onChoose);

  /** A button's choice; a click a key made before the keys arm chooses nothing. */
  const here = (choice: StarChoice, event: { detail: number }) => () => {
    if (keys.pressed(event)) onChoose(choice);
    return undefined;
  };

  return <div ref={placed} className="dialog-backdrop" role="presentation">
    <section
      ref={ref}
      tabIndex={-1}
      className="confirm-dialog star-dialog"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="star-title"
      aria-describedby="star-body"
      data-testid="star-prompt-dialog"
      data-keys-ready={keys.ready}
      onMouseDown={(event) => event.stopPropagation()}
    >
      <div className="star-icon"><Star size={20} /></div>
      <h2 id="star-title">{STAR_TITLE}</h2>
      <p id="star-body">{STAR_BODY_FIRST}<br />{STAR_BODY_SECOND}</p>
      <p className="star-repo">{STAR_REPO_DISPLAY}</p>
      <div>
        <button className="button secondary" onClick={(event) => VOICE_ACTIONS.dismissStarPrompt.run({ dismissStarPrompt: here("dismiss", event) })}><kbd>d</kbd> {DONT_ASK_AGAIN}</button>
        <button className="button secondary" onClick={(event) => VOICE_ACTIONS.remindStarLater.run({ remindStarLater: here("later", event) })}><kbd>l</kbd> {LATER}</button>
        <button className="button primary" onClick={(event) => VOICE_ACTIONS.starRepo.run({ starRepo: here("star", event) })}><kbd>s</kbd> {STAR}</button>
      </div>
    </section>
  </div>;
}
