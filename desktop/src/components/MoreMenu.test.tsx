import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MENU_MARGIN, MoreMenu, placeMenu } from "./MoreMenu";

/*
 * PR #2491 decision 2026-11-25 — the ⋯ ("More") menu every daemon header, the
 * top bar and a Table row share (`desktop/agent-cards/110`).
 */

function Harness({ onCard, onTick }: { onCard: () => void; onTick: () => void }) {
  const [ticked, setTicked] = useState(false);
  return (
    <div>
      <button type="button">Before</button>
      <MoreMenu
        label="More for build box"
        menuLabel="More for build box"
        testId="more"
        items={[
          { key: "card-actions", label: "Card actions", onSelect: onCard },
          { key: "needs-you", label: "Needs you only", checked: ticked, onSelect: () => { setTicked(!ticked); onTick(); } },
          { key: "later", label: "Later item", onSelect: vi.fn() },
        ]}
      />
      <button type="button">Outside</button>
    </div>
  );
}

const button = () => screen.getByTestId("more");
const menu = () => screen.queryByRole("menu", { name: "More for build box" });
const items = () => [...menu()!.querySelectorAll<HTMLElement>("[role='menuitem'], [role='menuitemcheckbox']")];
const focused = () => document.activeElement?.textContent;
const key = (target: Element, name: string) => act(() => { fireEvent.keyDown(target, { key: name }); });

afterEach(cleanup);

describe("the ⋯ menu (PR #2491 decision 2026-11-25)", () => {
  /// Scenario: Focus a ⋯ button and use only the keyboard. Enter, Space and ArrowDown each open its menu with the first item focused and ArrowUp opens it on the last; the button says it has a menu and whether it is open; the menu is a menu of menu items, the checkable one carrying its state; ArrowDown and ArrowUp move through the items and wrap, Home and End go to the ends; Escape closes it and puts focus back on ⋯; Tab closes it; a press outside closes it; and choosing an item closes the menu, returns focus to ⋯ and does what the item does.
  it("desktop/agent-cards/110: the ⋯ menu opens, moves and closes from the keyboard, with menu roles", () => {
    const onCard = vi.fn();
    const onTick = vi.fn();
    render(<Harness onCard={onCard} onTick={onTick} />);
    expect(button()).toHaveAccessibleName("More for build box");
    expect(button()).toHaveAttribute("title", "More for build box");
    expect(button()).toHaveAttribute("aria-haspopup", "menu");
    expect(button()).toHaveAttribute("aria-expanded", "false");
    expect(menu()).toBeNull();

    for (const opener of ["Enter", " ", "ArrowDown"]) {
      button().focus();
      key(button(), opener);
      expect(button(), opener).toHaveAttribute("aria-expanded", "true");
      expect(menu(), opener).not.toBeNull();
      expect(button().getAttribute("aria-controls")).toBe(menu()!.id);
      expect(focused(), opener).toBe("Card actions");
      key(document.activeElement!, "Escape");
      expect(menu(), opener).toBeNull();
      expect(document.activeElement, `${opener}: focus back on ⋯`).toBe(button());
    }

    key(button(), "ArrowUp");
    expect(focused()).toBe("Later item");
    key(document.activeElement!, "Escape");

    key(button(), "Enter");
    expect(items().map((item) => [item.getAttribute("role"), item.getAttribute("aria-checked")])).toEqual([["menuitem", null], ["menuitemcheckbox", "false"], ["menuitem", null]]);
    key(document.activeElement!, "ArrowDown");
    expect(focused()).toBe("Needs you only");
    key(document.activeElement!, "ArrowDown");
    expect(focused()).toBe("Later item");
    key(document.activeElement!, "ArrowDown");
    expect(focused(), "wraps to the first").toBe("Card actions");
    key(document.activeElement!, "ArrowUp");
    expect(focused(), "wraps to the last").toBe("Later item");
    key(document.activeElement!, "Home");
    expect(focused()).toBe("Card actions");
    key(document.activeElement!, "End");
    expect(focused()).toBe("Later item");
    key(document.activeElement!, "Tab");
    expect(menu(), "Tab closes it").toBeNull();

    fireEvent.click(button());
    expect(menu()).not.toBeNull();
    act(() => { fireEvent.pointerDown(screen.getByRole("button", { name: "Outside" })); });
    expect(menu(), "a press outside closes it").toBeNull();

    key(button(), "Enter");
    key(document.activeElement!, "ArrowDown");
    act(() => { fireEvent.click(document.activeElement!); });
    expect(onTick).toHaveBeenCalledOnce();
    expect(menu()).toBeNull();
    expect(document.activeElement).toBe(button());
    key(button(), "Enter");
    expect(items()[1]).toHaveAttribute("aria-checked", "true");
    act(() => { fireEvent.click(items()[0]); });
    expect(onCard).toHaveBeenCalledOnce();
    expect(menu()).toBeNull();
  });
});

describe("where a ⋯ menu goes (PR #2491, the 375 px reds)", () => {
  const phone = { width: 375, height: 900 };
  const box = (left: number, top: number, width: number, height: number) => ({ left, top, width, height });

  /// Scenario: A daemon's ⋯ sits near the left edge of a 375 px window, and its menu, right-aligned to the button by the stylesheet, would start 105 px left of the window. It flips to start at the button's left edge instead and stays where it was down the page.
  it("desktop/agent-cards/110: a menu past the left edge flips to the button's left edge", () => {
    const button = box(40, 100, 30, 24);
    const spot = placeMenu(button, box(-105, 128, 175, 40), phone);
    expect(spot).toEqual({ left: 40, top: 128, width: 175, height: 40 });
  });

  /// Scenario: The selection bar's Edit card actions sits near the right edge of a 375 px window, and its menu, left-aligned and opening upward, would end 6 px past the window. It flips to end at the button's right edge and still opens upward.
  it("desktop/agent-cards/117: a menu past the right edge flips to the button's right edge, keeping its side", () => {
    const button = box(330, 820, 28, 26);
    const spot = placeMenu(button, box(330, 734, 51, 80), phone, 6);
    expect(spot).toEqual({ left: 307, top: 734, width: 51, height: 80 });
  });

  /// Scenario: A menu that fits neither edge of its button is clamped to the window with the margin, a menu wider than the window is capped to it, and one that fits is left where the stylesheet put it.
  it("desktop/agent-cards/110: a menu that fits neither edge is clamped, a wide one is capped, and one that fits stays", () => {
    expect(placeMenu(box(180, 100, 20, 24), box(180, 128, 300, 40), phone).left).toBe(375 - MENU_MARGIN - 300);
    const wide = placeMenu(box(180, 100, 20, 24), box(0, 128, 600, 40), phone);
    expect(wide.width).toBe(375 - 2 * MENU_MARGIN);
    expect(wide.left).toBe(MENU_MARGIN);
    expect(placeMenu(box(1200, 100, 30, 24), box(1080, 128, 150, 40), { width: 1440, height: 900 })).toEqual({ left: 1080, top: 128, width: 150, height: 40 });
  });

  /// Scenario: A menu that would run off the foot of the window opens above its button when there is room there, an upward menu with no room above opens below, and a menu too tall for either side takes the larger side and is capped to it.
  it("desktop/agent-cards/110: a menu flips up or down to stay in the window, and is capped when neither side fits", () => {
    expect(placeMenu(box(100, 850, 30, 24), box(100, 878, 150, 120), phone).top).toBe(850 - 4 - 120);
    expect(placeMenu(box(100, 20, 30, 24), box(100, -90, 150, 106), phone).top).toBe(48);
    const tall = placeMenu(box(100, 300, 30, 24), box(100, 328, 150, 2000), phone);
    expect(tall.top).toBe(328);
    expect(tall.height).toBe(900 - MENU_MARGIN - 328);
  });
});
