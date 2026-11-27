import { useEffect, useId, useRef, useState, type ButtonHTMLAttributes, type CSSProperties, type FocusEvent, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";

/** The gap between the button and its tooltip, and the tooltip's margin from the window's edges, in pixels. */
const GAP = 6;
const EDGE = 8;
/** The widest the tooltip is drawn (the stylesheet's `max-width`), for keeping it inside the window. */
const TIP_WIDTH = 320;

/** What the tooltip says, under the text, when the window has no room for all of it. */
const TIP_CUT_NOTE = "Too long to show here in full: pressing it shows all of it before anything is sent.";

/**
 * The size `tip` takes drawn as the tooltip, at most `width` wide: measured
 * from a hidden copy with the tooltip's own class, so line breaks, wrapping
 * and the stylesheet's padding all count (a renderer with no layout measures
 * nothing, and every tooltip fits there).
 */
function tipSize(tip: string, width: number): { width: number; height: number } {
  const probe = document.createElement("div");
  probe.className = "action-tip";
  probe.setAttribute("aria-hidden", "true");
  probe.style.visibility = "hidden";
  probe.style.left = "0";
  probe.style.top = "0";
  probe.style.maxWidth = `${width}px`;
  probe.textContent = tip;
  document.body.appendChild(probe);
  const size = probe.getBoundingClientRect();
  probe.remove();
  return { width: size.width, height: size.height };
}

/** Where a tooltip is drawn and whether all of it fits there. */
export interface TipLayout {
  style: CSSProperties;
  /** Whether the whole text fits in the window; when not, the tooltip ends with {@link TIP_CUT_NOTE}. */
  whole: boolean;
}

/**
 * Issue #2399 re-review N2 — where the tooltip of a button at `box` is drawn,
 * from its real size, starting at the button's left edge (PR #2491 v9 review): below the button when it fits there, else above when it
 * fits there, else on the side with more room, never past any edge of the
 * window (its height capped to the room on that side, its width to the
 * window's). `whole` is false when the text does not fit on either side,
 * however few characters it has: many line breaks count as much as length.
 */
function tipLayout(box: DOMRect, tip: string): TipLayout {
  const width = Math.min(TIP_WIDTH, window.innerWidth - 2 * EDGE);
  const drawn = tipSize(tip, width);
  const height = drawn.height;
  /* Started at the icon's left edge and moved left only as far as its own
     drawn width needs to stay in the window (PR #2491 v9 review), so a short
     tooltip near the window's edge stays beside its icon. */
  const left = Math.max(EDGE, Math.min(box.left, window.innerWidth - EDGE - Math.min(width, drawn.width || width)));
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
 * {@link TIP_CUT_NOTE}. While it is shown it is measured again when its text
 * changes, on a window resize or scroll, and when the button's size changes
 * (final review F1). `onClick` is told whether the whole text fits beside the
 * button (`whole`), by pointer or keyboard alike: true only when it fits at
 * the press and the tooltip was not drawn cut since it was last shown (final
 * audit A4), so a caller can show the whole text first otherwise.
 */
export function ActionTipButton({ tip, unavailable = false, onClick, onFocus, onBlur, onKeyDown, onMouseEnter, onMouseLeave, ...button }: Omit<ButtonHTMLAttributes<HTMLButtonElement>, "disabled" | "title" | "type" | "onClick"> & {
  /** The explanation: the tooltip's text and the button's accessible description. */
  tip: string;
  unavailable?: boolean;
  onClick?: (event: MouseEvent<HTMLButtonElement>, shown: { whole: boolean }) => void;
}) {
  const id = useId();
  const own = useRef<HTMLButtonElement>(null);
  const [at, setAt] = useState<TipLayout | undefined>(undefined);
  const [hovered, setHovered] = useState(false);
  const [focused, setFocused] = useState(false);
  // Final audit A4: whether the tooltip has been drawn cut since it was last
  // shown. A press then asks for the whole text even when the window has
  // since grown to hold it: nothing says the newly drawn text was read.
  const cutSinceShown = useRef(false);
  const draw = (layout: TipLayout) => {
    if (!layout.whole) cutSinceShown.current = true;
    setAt(layout);
  };
  const close = () => {
    cutSinceShown.current = false;
    setAt(undefined);
  };
  const place = (target: HTMLButtonElement) => draw(tipLayout(target.getBoundingClientRect(), tip));
  const shown = at !== undefined;
  // Final review F1: a tooltip that is shown is measured again when its text
  // changes, when the window is resized or anything scrolls, and when the
  // button's own size changes, so its place and its cut note stay true.
  // PR #2491 v9 review: and whenever the button moves for any other reason —
  // the page reflowing as its fonts settle, a card above it growing — checked
  // once a frame while the tooltip is shown, so it stays beside its icon.
  useEffect(() => {
    const button = own.current;
    if (!shown || !button) return;
    let last = "";
    const again = (force: boolean) => {
      const box = button.getBoundingClientRect();
      const at = `${box.left},${box.top},${box.width},${box.height},${window.innerWidth},${window.innerHeight}`;
      if (!force && at === last) return;
      last = at;
      draw(tipLayout(box, tip));
    };
    const measure = () => again(true);
    again(true);
    window.addEventListener("resize", measure);
    window.addEventListener("scroll", measure, true);
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(measure);
    observer?.observe(button);
    let frame = 0;
    const follow = () => { again(false); frame = requestAnimationFrame(follow); };
    if (typeof requestAnimationFrame === "function") frame = requestAnimationFrame(follow);
    return () => {
      window.removeEventListener("resize", measure);
      window.removeEventListener("scroll", measure, true);
      observer?.disconnect();
      if (frame && typeof cancelAnimationFrame === "function") cancelAnimationFrame(frame);
    };
  }, [shown, tip]);
  const show = (event: FocusEvent<HTMLButtonElement>) => {
    onFocus?.(event);
    setFocused(true);
    place(event.currentTarget);
  };
  const hide = (event: FocusEvent<HTMLButtonElement>) => {
    onBlur?.(event);
    setFocused(false);
    if (!hovered) close();
  };
  const enter = (event: MouseEvent<HTMLButtonElement>) => {
    onMouseEnter?.(event);
    setHovered(true);
    place(event.currentTarget);
  };
  const leave = (event: MouseEvent<HTMLButtonElement>) => {
    onMouseLeave?.(event);
    setHovered(false);
    if (!focused) close();
  };
  const key = (event: KeyboardEvent<HTMLButtonElement>) => {
    onKeyDown?.(event);
    if (event.key === "Escape" && at) close();
  };
  const press = (event: MouseEvent<HTMLButtonElement>) => {
    // Whole only when it fits now and was not drawn cut while shown: a
    // measurement at the press never turns a cut tooltip into a direct send.
    const whole = tipLayout(event.currentTarget.getBoundingClientRect(), tip).whole && !cutSinceShown.current && (at?.whole ?? true);
    close();
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
        ref={own}
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

/**
 * PR #2491 review — the box around a set of action icons: a card's own, the
 * selection bar's and the agent pane's Card tab all draw their icons
 * inside one, so a user who learns where a card's actions are finds them in
 * the same box in all three. A card's own Close stays outside its group; the
 * batch's Close, in the bar, is one of the
 * batch's actions and sits inside.
 */
export function ActionIconGroup({ className = "", label, children }: { className?: string; label: string; children: ReactNode }) {
  return <span className={`action-icon-group${className ? ` ${className}` : ""}`} role="group" aria-label={label} data-testid="action-icon-group">{children}</span>;
}
