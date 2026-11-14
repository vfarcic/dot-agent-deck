import { useCallback, useEffect, useLayoutEffect, useState, type KeyboardEvent } from "react";
import { Star } from "lucide-react";

import { insideVoicePeer, overlayAbove, OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import {
  DONT_ASK_AGAIN,
  LATER,
  publishStarPrompt,
  STAR,
  STAR_BODY_FIRST,
  STAR_BODY_SECOND,
  STAR_PROMPT_COVERED,
  STAR_REPO_DISPLAY,
  STAR_TITLE,
  type StarChoice,
} from "../lib/starPrompt";
import { VOICE_ACTIONS } from "../lib/voiceActions";

/** The key that makes each choice, the TUI prompt's. */
const KEYS: Record<string, StarChoice> = { s: "star", l: "later", Escape: "later", d: "dismiss" };

/**
 * PRD #2174 — the star-repo reminder: the TUI's star prompt, in its words and
 * with its keys. `s` (Star) opens the repository page and stops the reminder,
 * `l` or `Esc` (Later) asks again after another ten launches, and `d` (Don't
 * ask again) stops it.
 *
 * On the overlay stack, so the rest of the window is fenced while it is open
 * and voice's `close` ("cancel", "never mind") is Later, as `Esc` is. Each
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

  /* The keys reach the dialog wherever focus was when it opened. */
  useEffect(() => { ref.current?.focus(); }, [ref]);

  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    const choice = KEYS[event.key];
    if (!choice) return;
    event.preventDefault();
    event.stopPropagation();
    onChoose(choice);
  };

  const here = (choice: StarChoice) => () => {
    onChoose(choice);
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
      onMouseDown={(event) => event.stopPropagation()}
      onKeyDown={onKeyDown}
    >
      <div className="star-icon"><Star size={20} /></div>
      <h2 id="star-title">{STAR_TITLE}</h2>
      <p id="star-body">{STAR_BODY_FIRST}<br />{STAR_BODY_SECOND}</p>
      <p className="star-repo">{STAR_REPO_DISPLAY}</p>
      <div>
        <button className="button secondary" onClick={() => VOICE_ACTIONS.dismissStarPrompt.run({ dismissStarPrompt: here("dismiss") })}><kbd>d</kbd> {DONT_ASK_AGAIN}</button>
        <button className="button secondary" onClick={() => VOICE_ACTIONS.remindStarLater.run({ remindStarLater: here("later") })}><kbd>l</kbd> {LATER}</button>
        <button className="button primary" onClick={() => VOICE_ACTIONS.starRepo.run({ starRepo: here("star") })}><kbd>s</kbd> {STAR}</button>
      </div>
    </section>
  </div>;
}
