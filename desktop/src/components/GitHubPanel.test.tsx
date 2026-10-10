import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { DEFAULT_DESKTOP_SETTINGS } from "../lib/bridge";
import { PrBrowserHostContext, unavailablePrBrowser, type PrBrowserHost } from "../lib/prBrowser";
import { GitHubPanel } from "./GitHubPanel";
import type { RuntimeMode } from "../types";

function host(signOut: PrBrowserHost["signOut"]): PrBrowserHost {
  return { ...unavailablePrBrowser(), available: true, signOut: vi.fn(signOut) };
}

function renderPanel(value: PrBrowserHost, mode: RuntimeMode = "live") {
  return render(
    <PrBrowserHostContext.Provider value={value}>
      <GitHubPanel settings={DEFAULT_DESKTOP_SETTINGS} onSave={vi.fn()} mode={mode} />
    </PrBrowserHostContext.Provider>,
  );
}

describe("Settings → GitHub (PRD #1401)", () => {
  /** Scenario: the user presses Sign out of GitHub; the browser's profile is cleared and the panel says so. */
  it("clears the pull request browser's sign-in", async () => {
    const value = host(async () => undefined);
    renderPanel(value);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Sign out of GitHub" })); });

    expect(value.signOut).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("status")).toHaveTextContent("Signed out. The next pull request you open asks you to sign in again.");
  });

  /** Scenario: the clear fails; the reason is shown and the button can be pressed again. */
  it("says why a sign-out failed", async () => {
    const value = host(async () => { throw new Error("the profile is busy"); });
    renderPanel(value);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Sign out of GitHub" })); });

    expect(screen.getByRole("alert")).toHaveTextContent("Could not sign out: the profile is busy");
    expect(screen.getByRole("button", { name: "Sign out of GitHub" })).toBeEnabled();
  });

  /** Scenario: Rust rejects with a plain string, as Tauri commands do; it is shown as it came. */
  it("shows a rejection that is a plain string", async () => {
    const value = host(() => Promise.reject("No sign-in profile."));
    renderPanel(value);
    await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Sign out of GitHub" })); });

    expect(screen.getByRole("alert")).toHaveTextContent("Could not sign out: No sign-in profile.");
  });

  /** Scenario: the browser preview has no in-app browser, so there is nothing to sign out of. */
  it("is unavailable in the browser preview", () => {
    renderPanel(unavailablePrBrowser(), "fixture");
    expect(screen.getByRole("button", { name: "Sign out of GitHub" })).toBeDisabled();
    expect(screen.getByText("Pull requests open in the desktop app, not in the browser preview.")).toBeVisible();
  });
});
