import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MoreMenu } from "./MoreMenu";

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
