import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { SelfUpgradeCheck, SelfUpgradePlan } from "../lib/selfUpgrade";
import { SelfUpgradeBanner, SelfUpgradeRailButton } from "./SelfUpgradeNotice";

const HEADLINE = "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)";

const plan = (action: SelfUpgradePlan["action"], headline: string): SelfUpgradePlan => ({
  copy: "app",
  label: "Agent Deck (desktop app)",
  headline,
  current: "0.46.0",
  latest: "0.47.0",
  action,
  actionable: action === "swap-app",
  confirmQuestion: null,
  provenance: { checked: true, reason: null },
  lines: [{ text: headline, command: null }],
});

const NEWER: SelfUpgradeCheck = { checkId: 1, latest: "0.47.0", updateAvailable: true, notice: HEADLINE, installed: null, app: plan("swap-app", HEADLINE), cli: null, recheckAfterSecs: 21600 };
const CURRENT: SelfUpgradeCheck = { checkId: 1, latest: "0.46.0", updateAvailable: false, notice: null, installed: null, app: plan("up-to-date", "Agent Deck (desktop app) is up to date (v0.46.0)."), cli: null, recheckAfterSecs: 21600 };
const RELAUNCH_NOTICE = "Agent Deck v0.47.0 is installed. Relaunch to run it.";
/* The app replaced itself and nothing else is behind: the bridge's notice is the relaunch prompt. */
const INSTALLED: SelfUpgradeCheck = {
  ...NEWER,
  updateAvailable: false,
  notice: RELAUNCH_NOTICE,
  installed: { copy: "app", ok: true, relaunch: true, lines: [{ text: "Replaced /Applications/Agent Deck.app with v0.47.0. Quit and reopen Agent Deck to run it.", command: null }] },
};

describe("SelfUpgradeNotice", () => {
  /** Scenario: Every copy runs the latest release, or no check has answered yet: neither the rail button nor the dashboard banner appears. */
  it("self_upgrade_notice_001 shows nothing unless a newer release exists", () => {
    const { container } = render(<><SelfUpgradeRailButton check={CURRENT} onOpen={vi.fn()} /><SelfUpgradeBanner check={CURRENT} onOpen={vi.fn()} onDismiss={vi.fn()} /><SelfUpgradeRailButton onOpen={vi.fn()} /><SelfUpgradeBanner onOpen={vi.fn()} onDismiss={vi.fn()} /></>);
    expect(container).toBeEmptyDOMElement();
  });

  /** Scenario: A newer release exists: the banner reads the plan's headline — the TUI badge's words — and both its Upgrade… button and the rail button open the dialog; the banner's Dismiss reports a dismissal. */
  it("self_upgrade_notice_002 shows the headline when newer and opens the dialog", () => {
    const onOpen = vi.fn();
    const onDismiss = vi.fn();
    render(<><SelfUpgradeRailButton check={NEWER} onOpen={onOpen} /><SelfUpgradeBanner check={NEWER} onOpen={onOpen} onDismiss={onDismiss} /></>);

    expect(screen.getByTestId("self-upgrade-banner")).toHaveTextContent(HEADLINE);
    expect(screen.getByTestId("self-upgrade-rail")).toHaveAccessibleName(HEADLINE);
    fireEvent.click(screen.getByTestId("self-upgrade-rail"));
    fireEvent.click(screen.getByTestId("self-upgrade-banner-open"));
    expect(onOpen).toHaveBeenCalledTimes(2);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  /** Scenario: The app replaced itself and still runs the old build, with nothing else behind: the notice turns into the relaunch prompt, its button reads Relaunch…, and both it and the rail button open the dialog, where Relaunch is. */
  it("self_upgrade_notice_003 turns into a relaunch prompt once the app was replaced", () => {
    const onOpen = vi.fn();
    render(<><SelfUpgradeRailButton check={INSTALLED} onOpen={onOpen} /><SelfUpgradeBanner check={INSTALLED} onOpen={onOpen} onDismiss={vi.fn()} /></>);
    expect(screen.getByTestId("self-upgrade-banner")).toHaveTextContent(RELAUNCH_NOTICE);
    expect(screen.getByTestId("self-upgrade-banner-open")).toHaveTextContent("Relaunch…");
    expect(screen.getByTestId("self-upgrade-rail")).toHaveAccessibleName(RELAUNCH_NOTICE);
    fireEvent.click(screen.getByTestId("self-upgrade-banner-open"));
    fireEvent.click(screen.getByTestId("self-upgrade-rail"));
    expect(onOpen).toHaveBeenCalledTimes(2);
  });
});
