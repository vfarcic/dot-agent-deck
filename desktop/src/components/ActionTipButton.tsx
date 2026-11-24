import { useId, useState, type ButtonHTMLAttributes, type CSSProperties, type FocusEvent, type KeyboardEvent, type MouseEvent } from "react";
import { createPortal } from "react-dom";

/** The gap between the button and its tooltip, and the tooltip's margin from the window's edges, in pixels. */
const GAP = 6;
const EDGE = 8;
/** The widest the tooltip is drawn (the stylesheet's `max-width`), for keeping it inside the window. */
const TIP_WIDTH = 320;

/** Where the tooltip of a button at `box` is drawn: below it, or above when the window has no room below; never past either side. */
function tipPosition(box: DOMRect): CSSProperties {
  const width = Math.min(TIP_WIDTH, window.innerWidth - 2 * EDGE);
  const left = Math.max(EDGE, Math.min(box.left, window.innerWidth - EDGE - width));
  const below = window.innerHeight - box.bottom;
  return below >= 120 || below >= box.top
    ? { left, top: box.bottom + GAP, maxWidth: width }
    : { left, bottom: window.innerHeight - box.top + GAP, maxWidth: width };
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
 */
export function ActionTipButton({ tip, unavailable = false, onClick, onFocus, onBlur, onKeyDown, onMouseEnter, onMouseLeave, ...button }: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "disabled" | "title" | "type"> & {
  /** The explanation: the tooltip's text and the button's accessible description. */
  tip: string;
  unavailable?: boolean;
}) {
  const id = useId();
  const [at, setAt] = useState<CSSProperties | undefined>(undefined);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  const place = (target: HTMLButtonElement) => setAt(tipPosition(target.getBoundingClientRect()));
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
    setAt(undefined);
    if (unavailable) {
      event.preventDefault();
      return;
    }
    onClick?.(event);
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
      {at && createPortal(<div role="tooltip" className="action-tip" data-testid="action-tip" style={at}>{tip}</div>, document.body)}
    </>
  );
}
