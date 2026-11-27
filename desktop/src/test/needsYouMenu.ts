import { act, fireEvent, screen, within } from "@testing-library/react";

/*
 * PR #2491 decision 2026-11-25 — Needs you only lives in the Dashboard's ⋯
 * ("More") menu as a checkable item, and while it is on a chip beside Filter
 * says so. The tests that pressed the Cards toolbar's old Needs you only
 * button press it here instead.
 */

/** The top bar's ⋯ button. */
export const dashboardMore = () => screen.getByRole("button", { name: "More" });

/** Open the top bar's ⋯ menu and press Needs you only, as a user does. */
export function pressNeedsYouOnly() {
  act(() => { fireEvent.click(dashboardMore()); });
  const menu = screen.getByRole("menu", { name: "More" });
  act(() => { fireEvent.click(within(menu).getByRole("menuitemcheckbox", { name: "Needs you only" })); });
}

/** Whether Needs you only is on, as its menu item says (the menu is opened to read it, then closed). */
export function needsYouOnlyChecked(): boolean {
  act(() => { fireEvent.click(dashboardMore()); });
  const checked = within(screen.getByRole("menu", { name: "More" })).getByRole("menuitemcheckbox", { name: "Needs you only" }).getAttribute("aria-checked") === "true";
  act(() => { fireEvent.click(dashboardMore()); });
  return checked;
}

/** Whether the Cards view is on screen: a daemon's cards are drawn (the Cards toolbar row that used to say so is gone). */
export const cardsViewShown = () => document.querySelector("[data-testid^='agent-cards-deck-']") !== null;
