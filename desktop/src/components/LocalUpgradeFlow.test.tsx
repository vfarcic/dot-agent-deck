import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { SelfUpgradeCheck, SelfUpgradePlan, SelfUpgradeResult } from "../lib/selfUpgrade";
import type { UpgradeOutcome } from "../lib/upgrade";
import { APP_BEFORE_CLI, LocalUpgradeFlow } from "./LocalUpgradeFlow";
import type { UpgradeTarget } from "./UpgradeDialog";

/**
 * Issue #1959 — Upgrade on the Local daemon section while the desktop app
 * itself is behind: the app's upgrade comes first, then the CLI's, and a
 * replaced app ends the flow at Relaunch instead of restarting the daemon
 * onto the build the old app still runs.
 */
describe("Upgrade on the Local daemon section while the app is behind (issue #1959)", () => {
  const plan = (copy: "app" | "cli", label: string, action: SelfUpgradePlan["action"]): SelfUpgradePlan => {
    const headline = `${label}: update available: v0.45.0 (current: v0.44.0)`;
    return {
      copy,
      label,
      headline,
      current: "0.44.0",
      latest: "0.45.0",
      action,
      actionable: true,
      confirmQuestion: `Upgrade ${label} to v0.45.0?`,
      provenance: { checked: true, reason: null },
      lines: [{ text: headline, command: null }],
    };
  };
  const appPlan = plan("app", "Agent Deck (desktop app)", "swap-app");
  const cliPlan = plan("cli", "dot-agent-deck CLI", "brew-upgrade");
  const check: SelfUpgradeCheck = {
    checkId: 4,
    latest: "0.45.0",
    updateAvailable: true,
    notice: appPlan.headline,
    installed: null,
    app: appPlan,
    cli: cliPlan,
    recheckAfterSecs: 21600,
  };
  const swapped: SelfUpgradeResult = { copy: "app", ok: true, installed: true, relaunch: true, lines: [{ text: "Replaced /Applications/Agent Deck.app with v0.45.0. Quit and reopen Agent Deck to run it.", command: null }] };
  const brewed: SelfUpgradeResult = { copy: "cli", ok: true, installed: true, relaunch: false, lines: [{ text: "`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.", command: null }] };
  const daemon: UpgradeTarget = { deckId: "local", deckName: "Local daemon", kind: "local-upgrade", offer: { kind: "offered", from: "0.43.0", to: "0.44.0" } };
  const runtime = () => ({
    upgradeDaemon: vi.fn(async () => ({ outcome: "restarted", fromVersion: "0.43.0", toVersion: "0.44.0", stopped: { agents: [], roles: [] } }) as UpgradeOutcome),
    decideUpgrade: vi.fn(async () => undefined),
  });

  /** Scenario: The app, the CLI and the local daemon are all behind. Upgrade on the Local daemon section shows the app's plan above the CLI's and says the app comes first; it asks about the app, then the CLI, and once the app was replaced it ends at Relaunch without restarting the daemon. */
  it("local_upgrade_011 offers the app before the CLI and leaves the daemon to the relaunched app", async () => {
    const api = { check: vi.fn(), run: vi.fn(async (copy: string) => (copy === "app" ? swapped : brewed)), relaunch: vi.fn(async () => undefined) };
    const deck = runtime();
    const onClose = vi.fn();
    render(<LocalUpgradeFlow daemon={daemon} cli={{ check, api }} runtime={deck} onClose={onClose} />);

    const plans = screen.getAllByTestId(/^self-upgrade-plan-/).map((section) => section.dataset.testid);
    expect(plans).toEqual(["self-upgrade-plan-app", "self-upgrade-plan-cli"]);
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent(APP_BEFORE_CLI);
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "app");

    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenLastCalledWith("app", 4);
    await screen.findByTestId("self-upgrade-result-app");
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenLastCalledWith("cli", 4);
    await screen.findByTestId("self-upgrade-result-cli");

    expect(screen.getByTestId("self-upgrade-relaunch")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: The app is behind and the user cancels its question: the whole Upgrade ends having changed nothing, so the CLI is never upgraded ahead of the app. */
  it("local_upgrade_012 Cancel on the app's question ends the whole upgrade", () => {
    const api = { check: vi.fn(), run: vi.fn(async () => brewed), relaunch: vi.fn() };
    const deck = runtime();
    const onClose = vi.fn();
    render(<LocalUpgradeFlow daemon={daemon} cli={{ check, api }} runtime={deck} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("self-upgrade-cancel"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(api.run).not.toHaveBeenCalled();
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: The app's upgrade fails, the CLI is then upgraded: with no relaunch waiting, closing the dialog goes on to restart the older local daemon, as it does when the app is current. */
  it("local_upgrade_013 an app that was not replaced goes on to the daemon after the CLI", async () => {
    const failed: SelfUpgradeResult = { copy: "app", ok: false, installed: false, relaunch: false, lines: [{ text: "Cannot download Agent Deck.", command: null }] };
    const api = { check: vi.fn(), run: vi.fn(async (copy: string) => (copy === "app" ? failed : brewed)), relaunch: vi.fn() };
    const deck = runtime();
    render(<LocalUpgradeFlow daemon={daemon} cli={{ check, api }} runtime={deck} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-app");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-cli");
    expect(screen.queryByTestId("self-upgrade-relaunch")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith("local", expect.any(Function)));
  });

  /** Scenario: The app is behind but cannot be upgraded from here (an app folder you cannot write), and the daemon is older: the Local daemon section's dialog covers the CLI alone, as when the app is current, and says the daemon restart follows. */
  it("local_upgrade_015 an app this app cannot upgrade leaves the CLI and the daemon as before", async () => {
    const manual: SelfUpgradePlan = { ...appPlan, action: "manual-download", actionable: false, confirmQuestion: null };
    const api = { check: vi.fn(), run: vi.fn(async () => brewed), relaunch: vi.fn() };
    const deck = runtime();
    render(<LocalUpgradeFlow daemon={daemon} cli={{ check: { ...check, app: manual }, api }} runtime={deck} onClose={vi.fn()} />);
    expect(screen.queryByTestId("self-upgrade-plan-app")).not.toBeInTheDocument();
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent("After the CLI, Agent Deck restarts the daemon on this machine");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenCalledWith("cli", 4);
    await screen.findByTestId("self-upgrade-result-cli");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith("local", expect.any(Function)));
  });

  /** Scenario: The app was upgraded earlier and waits for a relaunch, and the CLI is behind: the dialog shows the app's result with Relaunch first, offers the CLI, and still leaves the daemon to the relaunched app. */
  it("local_upgrade_014 an app waiting for a relaunch still comes first", async () => {
    const waiting: SelfUpgradeCheck = { ...check, updateAvailable: false, installed: swapped };
    const api = { check: vi.fn(), run: vi.fn(async () => brewed), relaunch: vi.fn() };
    const deck = runtime();
    const onClose = vi.fn();
    render(<LocalUpgradeFlow daemon={daemon} cli={{ check: waiting, api }} runtime={deck} onClose={onClose} />);
    expect(screen.getByTestId("self-upgrade-result-app")).toHaveTextContent("Quit and reopen Agent Deck");
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent(APP_BEFORE_CLI);
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-cli");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });
});
