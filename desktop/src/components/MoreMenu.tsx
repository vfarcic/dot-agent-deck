import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
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
              title={item.title}
              onClick={() => { close(true); item.onSelect(); }}
            >
              {item.checked !== undefined && <span className="more-menu-mark" aria-hidden="true">{item.checked ? <Check size={13} /> : null}</span>}
              <span>{item.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
