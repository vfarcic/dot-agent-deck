import { useCallback, useLayoutEffect, useState } from "react";
import { FlaskConical } from "lucide-react";

import { insideVoicePeer, overlayAbove, OVERLAY_LAYER, useInertBackground, useNoOverlayAbove } from "../hooks/useInertBackground";
import { useReminderKeys } from "../hooks/useReminderKeys";
import {
  ANTIGRAVITY_NOTICE_COVERED,
  DONT_SHOW_AGAIN,
  NOTICE_BODY,
  NOTICE_ISSUES_DISPLAY,
  NOTICE_KEYS_ARM_MS,
  NOTICE_LATER,
  NOTICE_TITLE,
  OPEN_ISSUES,
  publishAntigravityNotice,
  type NoticeChoice,
} from "../lib/antigravityNotice";
import { VOICE_ACTIONS } from "../lib/voiceActions";

/**
 * PRD #211 — whether the notice is up: once it is `due` and `ready` (the
 * shell's list of what it waits behind) and nothing is drawn above an agent's
 * pane. Unlike the star reminder, it may come up over the pane: it follows a
 * start the user just made, and the new agent's pane is where they are. It
 * waits behind anything over that pane — the New agent dialog still closing,
 * a confirmation, the star reminder — so only one such dialog shows at a
 * time. Once up it stays up until answered, since it is then on the stack.
 */
export function AntigravityNoticeGate({ due, ready, onChoose }: { due: boolean; ready: boolean; onChoose: (choice: NoticeChoice) => void }) {
  const nothingAbovePane = useNoOverlayAbove(OVERLAY_LAYER.pane);
  const [up, setUp] = useState(false);
  const mayOpen = due && ready && nothingAbovePane;
  if (mayOpen && !up) setUp(true);
  if (!due && up) setUp(false);
  return due && (up || mayOpen) ? <AntigravityNoticeDialog onChoose={onChoose} /> : null;
}

/** The key that makes each choice, the TUI notice's. */
const KEYS: Record<string, NoticeChoice> = { o: "open", l: "later", Escape: "later", d: "dismiss" };

/**
 * PRD #211 — the Antigravity experimental notice: the TUI's notice, in its
 * words and with its keys. `o` (Open issues) opens the repository's new-issue
 * page, `l` or `Esc` (Later) closes it until the next interval, and `d`
 * (Don't show again) stops it.
 *
 * On the overlay stack, so the agent's pane behind it is fenced while it is
 * open, and focus goes back to where it was when the notice closes. It owns
 * the keyboard ({@link useReminderKeys}): keys reach neither the agent nor
 * the window shortcuts that act behind it (`?`, the digits, `Ctrl+K`), the
 * one that dismisses it included. Voice's `close`
 * ("cancel", "never mind") is Later, as `Esc` is. Each button dispatches
 * through the voice action registry (CLAUDE.md rule 23):
 * `openAntigravityIssues` and `dismissAntigravityNotice` are the
 * `open_antigravity_issues` and `dismiss_antigravity_notice` rows, and Later
 * is `close`'s.
 */
export function AntigravityNoticeDialog({ onChoose }: { onChoose: (choice: NoticeChoice) => void }) {
  const later = useCallback(() => onChoose("later"), [onChoose]);
  const [inVoicePeer, setInVoicePeer] = useState<boolean>();
  const placed = useCallback((node: HTMLDivElement | null) => { if (node) setInVoicePeer(insideVoicePeer(node)); }, []);
  const ref = useInertBackground<HTMLElement>(inVoicePeer === false, OVERLAY_LAYER.dialog, later);

  /* Voice's choices, published while the dialog is on screen, in the commit
     that draws it and taken back in the one that removes it. */
  useLayoutEffect(() => publishAntigravityNotice({
    choose: (choice) => {
      if (overlayAbove(ref.current)) return ANTIGRAVITY_NOTICE_COVERED;
      onChoose(choice);
      return undefined;
    },
  }), [onChoose, ref]);

  /* The keys choose only once it has been on screen for `NOTICE_KEYS_ARM_MS`:
     it opens on its own right after a start, so a key on its way to the new
     agent must not answer it. A held key's repeats never choose. Focus is
     moved inside by the overlay stack, which first notes where it was so that
     closing gives it back. */
  const keys = useReminderKeys(ref, KEYS, NOTICE_KEYS_ARM_MS, onChoose);

  /** A button's choice; a click a key made before the keys arm chooses nothing. */
  const here = (choice: NoticeChoice, event: { detail: number }) => () => {
    if (keys.pressed(event)) onChoose(choice);
    return undefined;
  };

  return <div ref={placed} className="dialog-backdrop" role="presentation">
    <section
      ref={ref}
      tabIndex={-1}
      className="confirm-dialog star-dialog antigravity-notice-dialog"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="antigravity-notice-title"
      aria-describedby="antigravity-notice-body"
      data-testid="antigravity-notice-dialog"
      data-keys-ready={keys.ready}
      onMouseDown={(event) => event.stopPropagation()}
    >
      <div className="star-icon"><FlaskConical size={20} /></div>
      <h2 id="antigravity-notice-title">{NOTICE_TITLE}</h2>
      <p id="antigravity-notice-body">{NOTICE_BODY}</p>
      <p className="star-repo">{NOTICE_ISSUES_DISPLAY}</p>
      <div>
        <button className="button secondary" onClick={(event) => VOICE_ACTIONS.dismissAntigravityNotice.run({ dismissAntigravityNotice: here("dismiss", event) })}><kbd>d</kbd> {DONT_SHOW_AGAIN}</button>
        <button className="button secondary" onClick={(event) => VOICE_ACTIONS.remindAntigravityLater.run({ remindAntigravityLater: here("later", event) })}><kbd>l</kbd> {NOTICE_LATER}</button>
        <button className="button primary" onClick={(event) => VOICE_ACTIONS.openAntigravityIssues.run({ openAntigravityIssues: here("open", event) })}><kbd>o</kbd> {OPEN_ISSUES}</button>
      </div>
    </section>
  </div>;
}
