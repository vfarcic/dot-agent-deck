import { useId, useState, type ButtonHTMLAttributes, type CSSProperties, type FocusEvent, type KeyboardEvent, type MouseEvent } from "react";
import { createPortal } from "react-dom";

/** The gap between the button and its tooltip, and the tooltip's margin from the window's edges, in pixels. */
const GAP = 6;
const EDGE = 8;
/** The widest the tooltip is drawn (the stylesheet's `max-width`), for keeping it inside the window. */
const TIP_WIDTH = 320;

/** What the tooltip says, under the text, when the window has no room for all of it. */
export const TIP_CUT_NOTE = "Too long to show here in full: pressing it shows all of it before anything is sent.";

/**
 * The height `tip` takes drawn as the tooltip, at most `width` wide: measured
 * from a hidden copy with the tooltip's own class, so line breaks, wrapping
 * and the stylesheet's padding all count (a renderer with no layout measures
 * nothing, and every tooltip fits there).
 */
function tipHeight(tip: string, width: number): number {
  const probe = document.createElement("div");
  probe.className = "action-tip";
  probe.setAttribute("aria-hidden", "true");
  probe.style.visibility = "hidden";
  probe.style.left = "0";
  probe.style.top = "0";
  probe.style.maxWidth = `${width}px`;
  probe.textContent = tip;
  document.body.appendChild(probe);
  const height = probe.getBoundingClientRect().height;
  probe.remove();
  return height;
}

/** Where a tooltip is drawn and whether all of it fits there. */
export interface TipLayout {
  style: CSSProperties;
  /** Whether the whole text fits in the window; when not, the tooltip ends with {@link TIP_CUT_NOTE}. */
  whole: boolean;
}

/**
 * Issue #2399 re-review N2 — where the tooltip of a button at `box` is drawn,
 * from its real size: below the button when it fits there, else above when it
 * fits there, else on the side with more room, never past any edge of the
 * window (its height capped to the room on that side, its width to the
 * window's). `whole` is false when the text does not fit on either side,
 * however few characters it has: many line breaks count as much as length.
 */
export function tipLayout(box: DOMRect, tip: string): TipLayout {
  const width = Math.min(TIP_WIDTH, window.innerWidth - 2 * EDGE);
  const left = Math.max(EDGE, Math.min(box.left, window.innerWidth - EDGE - width));
  const height = tipHeight(tip, width);
  const below = window.innerHeight - box.bottom - GAP - EDGE;
  const above = box.top - GAP - EDGE;
  const whole = height <= Math.max(below, above);
  const downward = height <= below || (height > above && below >= above);
  const room = Math.max(0, downward ? below : above);
  return {
    whole,
    style: downward
      ? { left, top: box.bottom + GAP, maxWidth: width, maxHeight: room }
      : { left, bottom: window.innerHeight - box.top + GAP, maxWidth: width, maxHeight: room },
  };
}

/**
 * Issue #2399 review (findings 7 and 10, audit 1) — an action's icon button
 * whose explanation (the action's label and the exact text it sends, and that
 * it asks first or why it is disabled) reaches the pointer and the keyboard
 * alike. Hovering it or focusing it draws the text as a tooltip over the page
 * (so no card clips it) until the pointer and focus have both left, Escape is
 * pressed or the button is pressed; the same text is always the button's
 * accessible description, so a screen reader reads it with the label whether
 * the tooltip is drawn or not. There is no native `title`, which would draw a
 * second tooltip over this one. `unavailable` marks an action the daemon
 * disabled: it is announced as disabled (`aria-disabled`) but keeps its place
 * in the Tab order and its explanation, which a native `disabled` button
 * would lose, and a press does nothing.
 *
 * The tooltip is sized from its text and kept inside the window on both axes
 * ({@link tipLayout}); a text the window has no room for ends, visibly, with
 * {@link TIP_CUT_NOTE}. `onClick` is told whether the whole text fits beside
 * the button (`whole`), measured when it is pressed, by pointer or keyboard
 * alike, so a caller can show the whole text first when it does not.
 */
export function ActionTipButton({ tip, unavailable = false, onClick, onFocus, onBlur, onKeyDown, onMouseEnter, onMouseLeave, ...button }: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "disabled" | "title" | "type" | "onClick"> & {
  /** The explanation: the tooltip's text and the button's accessible description. */
  tip: string;
  unavailable?: boolean;
  onClick?: (event: MouseEvent<HTMLButtonElement>, shown: { whole: boolean }) => void;
}) {
  const id = useId();
  const [at, setAt] = useState<TipLayout | undefined>(undefined);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const place = (target: HTMLButtonElement) => setAt(tipLayout(target.getBoundingClientRect(), tip));
  const show = (event: FocusEvent<HTMLButtonElement>) => {
    onFocus?.(event);
    setFocused(true);
    place(event.currentTarget);
  };
  const hide = (event: FocusEvent<HTMLButtonElement>) => {
    onBlur?.(event);
    setFocused(false);
    if (!hovered) setAt(undefined);
  };
  const enter = (event: MouseEvent<HTMLButtonElement>) => {
    onMouseEnter?.(event);
    setHovered(true);
    place(event.currentTarget);
  };
  const leave = (event: MouseEvent<HTMLButtonElement>) => {
    onMouseLeave?.(event);
    setHovered(false);
    if (!focused) setAt(undefined);
  };
  const key = (event: KeyboardEvent<HTMLButtonElement>) => {
    onKeyDown?.(event);
    if (event.key === "Escape" && at) setAt(undefined);
  };
  const press = (event: MouseEvent<HTMLButtonElement>) => {
    const { whole } = tipLayout(event.currentTarget.getBoundingClientRect(), tip);
    setAt(undefined);
    if (unavailable) {
      event.preventDefault();
      return;
    }
    onClick?.(event, { whole });
  };
  return (
    <>
      <button
        {...button}
        type="button"
        aria-disabled={unavailable ? true : undefined}
        aria-describedby={id}
        data-tip={tip}
        onFocus={show}
        onBlur={hide}
        onMouseEnter={enter}
        onMouseLeave={leave}
        onKeyDown={key}
        onClick={press}
      />
      {/* The description lives outside the card, so it is never part of the card's own text. */}
      {createPortal(<span id={id} hidden>{tip}</span>, document.body)}
      {at && createPortal(
        <div role="tooltip" className={`action-tip${at.whole ? "" : " is-cut"}`} data-testid="action-tip" style={at.style}>
          <span className="action-tip-text">{tip}</span>
          {!at.whole && <span className="action-tip-cut" data-testid="action-tip-cut">{TIP_CUT_NOTE}</span>}
        </div>,
        document.body,
      )}
    </>
  );
}
