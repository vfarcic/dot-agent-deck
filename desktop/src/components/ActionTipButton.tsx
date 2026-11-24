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
 * Issue #2399 review (finding 7) — an action's icon button whose explanation
 * (the action's label and the exact text it sends, and that it asks first or
 * why it is disabled) reaches the keyboard as well as the pointer. A pointer
 * gets the native `title`; keyboard focus shows the same text as a tooltip
 * drawn over the page (so no card clips it), which the button names as its
 * description, until focus leaves, Escape is pressed or the button is pressed
 * (a pointer press focuses it in some browsers). `unavailable` marks an
 * action the daemon disabled: it is announced as disabled (`aria-disabled`)
 * but keeps its place in the Tab order and its explanation, which a native
 * `disabled` button would lose, and a press does nothing.
 */
export function ActionTipButton({ tip, unavailable = false, onClick, onFocus, onBlur, onKeyDown, ...button }: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "disabled" | "title" | "type"> & {
  /** The explanation: the native hover's text and the tooltip's. */
  tip: string;
  unavailable?: boolean;
}) {
  const id = useId();
  const [at, setAt] = useState<CSSProperties | undefined>(undefined);
  const show = (event: FocusEvent<HTMLButtonElement>) => {
    onFocus?.(event);
    setAt(tipPosition(event.currentTarget.getBoundingClientRect()));
  };
  const hide = (event: FocusEvent<HTMLButtonElement>) => {
    onBlur?.(event);
    setAt(undefined);
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
        title={tip}
        aria-disabled={unavailable ? true : undefined}
        aria-describedby={at ? id : undefined}
        onFocus={show}
        onBlur={hide}
        onKeyDown={key}
        onClick={press}
      />
      {at && createPortal(<div id={id} role="tooltip" className="action-tip" style={at}>{tip}</div>, document.body)}
    </>
  );
}
