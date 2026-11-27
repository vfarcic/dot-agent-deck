import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { Check, Ellipsis } from "lucide-react";

/**
 * One item of a {@link MoreMenu}. With `checked` set it is a checkable item
 * (`menuitemcheckbox`, its state in `aria-checked`); otherwise a plain
 * `menuitem`. `onSelect` is what pressing it does, and the host routes it
 * through the action registry (CLAUDE.md rule 23).
 */
export interface MoreMenuItem {
  key: string;
  label: string;
  onSelect: () => void;
  checked?: boolean;
  disabled?: boolean;
  title?: string;
  testId?: string;
}

/** The items of a menu, as `role="menu"` children reach them by the keyboard. */
function menuItems(menu: HTMLElement | null): HTMLElement[] {
  if (!menu) return [];
  return [...menu.querySelectorAll<HTMLElement>("[role='menuitem'], [role='menuitemcheckbox']")].filter((item) => !(item as HTMLButtonElement).disabled);
}

/** The space a ⋯ menu keeps between itself and the window's edges, in px. */
export const MENU_MARGIN = 8;

/** A box in window coordinates: `left`, `top`, `width`, `height`. */
export interface MenuBox { left: number; top: number; width: number; height: number }

/**
 * Where a ⋯ menu goes so it stays inside the window, from the button's box,
 * the menu's natural size and the place its stylesheet gave it (PR #2491, the
 * 375 px reds of `desktop/agent-cards/110` and `117`). Returns the menu's
 * left and top in window coordinates, and the width and height it may take.
 *
 * - Across: the stylesheet's place when it fits; else aligned to the other
 *   edge of the button (the menu flips over it); else clamped to the window
 *   with {@link MENU_MARGIN}. A menu wider than the window is capped to it.
 * - Down: the stylesheet's side of the button (below, or above for a menu at
 *   the foot of the window) when it fits; else the other side when that one
 *   does; else whichever side has more room, capped to that room.
 */
export function placeMenu(button: MenuBox, natural: MenuBox, viewport: { width: number; height: number }, gap = 4): MenuBox {
  const room = Math.max(0, viewport.width - 2 * MENU_MARGIN);
  const width = Math.min(natural.width, room);
  const fitsAcross = (left: number) => left >= MENU_MARGIN && left + width <= viewport.width - MENU_MARGIN;
  const alignRight = button.left + button.width - width;
  const alignLeft = button.left;
  let left = natural.left;
  if (!fitsAcross(left)) left = [alignRight, alignLeft].find(fitsAcross) ?? Math.min(Math.max(left, MENU_MARGIN), viewport.width - MENU_MARGIN - width);
  const above = natural.top + natural.height <= button.top;
  const below = button.top + button.height + gap;
  const roomBelow = viewport.height - MENU_MARGIN - below;
  const roomAbove = button.top - gap - MENU_MARGIN;
  const fitsBelow = natural.height <= roomBelow;
  const fitsAbove = natural.height <= roomAbove;
  const goAbove = above ? (fitsAbove || (!fitsBelow && roomAbove >= roomBelow)) : (!fitsBelow && (fitsAbove || roomAbove > roomBelow));
  const height = Math.min(natural.height, Math.max(0, goAbove ? roomAbove : roomBelow));
  const top = goAbove ? button.top - gap - height : below;
  return { left, top, width, height };
}

/**
 * PR #2491 decision 2026-11-25 — the app's ⋯ ("More") menu: a button that
 * opens a small menu of items, used by every daemon section's header, by the
 * Dashboard's top bar and by a Table row (`OverviewRowCard`). One component,
 * so every ⋯ behaves the same and a later item is one more entry in `items`.
 *
 * - The button carries `aria-haspopup="menu"` and `aria-expanded`; the menu is
 *   `role="menu"` and its items `menuitem` or `menuitemcheckbox`.
 * - Enter, Space or ArrowDown on the button opens it with focus on the first
 *   item (ArrowUp, on the last); a click opens it the same way. ArrowDown and
 *   ArrowUp move through the items and wrap, Home and End go to the ends.
 * - Escape closes it and gives focus back to the button; Tab closes it and
 *   moves on. Pressing an item closes the menu, gives focus back to the
 *   button and then does what the item does, so a dialog it opens takes focus
 *   after it.
 * - A press anywhere outside it closes it, and so does an Escape raised
 *   elsewhere, caught in the capture phase as the Columns menu does it (issue
 *   #957), without stopping that Escape.
 *
 * Opening and closing is a view toggle and stays off voice: voice reaches the
 * items' own registry entries directly. `open` and `onToggle` make the open
 * state the host's (a Table row's menu, which the cards view holds); without
 * them the menu holds it.
 */
export function MoreMenu({ label, title, items, menuLabel, testId, menuTestId, className, buttonClassName, icon, badge, describedBy, open: heldOpen, onToggle }: {
  /** The button's accessible name. */
  label: string;
  /** Its hover text; the accessible name when absent. */
  title?: string;
  items: readonly MoreMenuItem[];
  /** The menu's accessible name. */
  menuLabel: string;
  testId?: string;
  menuTestId?: string;
  className?: string;
  buttonClassName?: string;
  icon?: ReactNode;
  /** Something small drawn on the button, such as a count (hidden from the accessible name; `describedBy` says it). */
  badge?: ReactNode;
  /** The id of an element whose text describes the button further. */
  describedBy?: string;
  /** The open state, when the host holds it. */
  open?: boolean;
  /** Ask the host to open or close the menu (with `open`). */
  onToggle?: (open: boolean) => void;
}) {
  const [ownOpen, setOwnOpen] = useState(false);
  const open = heldOpen ?? ownOpen;
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const menuId = useId();
  /* Which item takes focus once the menu is drawn: the first, the last, or none (a host opened it). */
  const focusOnOpen = useRef<"first" | "last" | undefined>(undefined);
  const setOpen = (next: boolean) => {
    if (onToggle) onToggle(next);
    else setOwnOpen(next);
  };
  const latestSetOpen = useRef(setOpen);
  latestSetOpen.current = setOpen;

  useEffect(() => {
    if (!open) return;
    const where = focusOnOpen.current;
    focusOnOpen.current = undefined;
    if (where === undefined) return;
    const list = menuItems(menu.current);
    (where === "last" ? list[list.length - 1] : list[0])?.focus();
  }, [open]);

  /* Keep the open menu inside the window on both axes: measured with the
     stylesheet's place first, then moved, flipped or capped by `placeMenu`.
     Again when the window resizes, since a menu that fitted may no longer. */
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const list = menu.current;
      const at = root.current;
      const opener = button.current;
      if (!list || !at || !opener) return;
      for (const key of ["left", "right", "top", "bottom", "maxWidth", "maxHeight", "overflowY"] as const) list.style[key] = "";
      const natural = list.getBoundingClientRect();
      const anchor = at.getBoundingClientRect();
      const doc = document.documentElement;
      const viewport = { width: doc.clientWidth || window.innerWidth, height: doc.clientHeight || window.innerHeight };
      const box = opener.getBoundingClientRect();
      /* The stylesheet's gap between button and menu, kept when the menu moves. */
      const gap = natural.top >= box.bottom ? natural.top - box.bottom : box.top - natural.bottom;
      const spot = placeMenu(box, natural, viewport, Math.min(Math.max(gap, 0), 12));
      list.style.maxWidth = `${spot.width}px`;
      list.style.left = `${spot.left - anchor.left}px`;
      list.style.right = "auto";
      list.style.top = `${spot.top - anchor.top}px`;
      list.style.bottom = "auto";
      if (spot.height < natural.height) {
        list.style.maxHeight = `${spot.height}px`;
        list.style.overflowY = "auto";
      }
    };
    place();
    window.addEventListener("resize", place);
    return () => window.removeEventListener("resize", place);
  }, [open, items]);

  /* A press or an Escape outside the menu closes it; the press inside it, its
     button included, is the menu's own (the Columns menu's rule, PRD #745). */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      latestSetOpen.current(false);
    };
    const dismissOnEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key === "Escape") dismiss(event);
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", dismissOnEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", dismissOnEscape, true);
    };
  }, [open]);

  const openWith = (where: "first" | "last") => {
    focusOnOpen.current = where;
    if (open) {
      const list = menuItems(menu.current);
      (where === "last" ? list[list.length - 1] : list[0])?.focus();
      focusOnOpen.current = undefined;
      return;
    }
    setOpen(true);
  };
  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) button.current?.focus();
  };
  const onButtonKey = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    if (event.key === "Enter" || event.key === " " || event.key === "ArrowDown") {
      event.preventDefault();
      if (open && event.key !== "ArrowDown") close(true);
      else openWith("first");
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      openWith("last");
    }
  };
  const onMenuKey = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    const list = menuItems(menu.current);
    const at = list.findIndex((item) => item === document.activeElement);
    const move = (to: number) => { event.preventDefault(); list[(to + list.length) % list.length]?.focus(); };
    if (event.key === "ArrowDown") move(at + 1);
    else if (event.key === "ArrowUp") move(at < 0 ? list.length - 1 : at - 1);
    else if (event.key === "Home") move(0);
    else if (event.key === "End") move(list.length - 1);
    else if (event.key === "Tab") close(false);
  };
  return (
    <div
      className={["overview-card-menu", className].filter(Boolean).join(" ")}
      ref={root}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !open) return;
        /* Closed here, and stopped, so the app-wide Escape does not also act on it. */
        event.preventDefault();
        event.stopPropagation();
        close(true);
      }}
    >
      <button
        ref={button}
        type="button"
        className={buttonClassName ?? "overview-card-menu-button"}
        data-testid={testId}
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        aria-describedby={describedBy}
        title={title ?? label}
        onClick={() => (open ? close(false) : openWith("first"))}
        onKeyDown={onButtonKey}
      >{icon ?? <Ellipsis size={12} aria-hidden="true" />}{badge}</button>
      {open && (
        <div id={menuId} ref={menu} className="overview-card-menu-list" role="menu" aria-label={menuLabel} data-testid={menuTestId} onKeyDown={onMenuKey}>
          {items.map((item) => (
            <button
              key={item.key}
              type="button"
              role={item.checked === undefined ? "menuitem" : "menuitemcheckbox"}
              aria-checked={item.checked}
              tabIndex={-1}
              className={item.checked === undefined ? undefined : "more-menu-check"}
              data-testid={item.testId}
              disabled={item.disabled}
              title={item.title ?? item.label}
              onClick={() => { close(true); item.onSelect(); }}
            >
              {item.checked !== undefined && <span className="more-menu-mark" aria-hidden="true">{item.checked ? <Check size={13} /> : null}</span>}
              <span className="more-menu-label">{item.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
