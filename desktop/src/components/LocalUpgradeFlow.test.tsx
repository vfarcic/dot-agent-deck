import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import type { SelfUpgradeCheck, SelfUpgradePlan, SelfUpgradeResult } from "../lib/selfUpgrade";
import type { UpgradeOutcome } from "../lib/upgrade";
import { DISPLAY_LIMITS } from "../lib/displayText";
import { APP_BEFORE_CLI, CLI_AFTER_APP, LocalUpgradeFlow, upgradeCopies } from "./LocalUpgradeFlow";
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
    render(<LocalUpgradeFlow daemon={daemon} copies={{ check, api }} runtime={deck} onClose={onClose} />);

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
    render(<LocalUpgradeFlow daemon={daemon} copies={{ check, api }} runtime={deck} onClose={onClose} />);
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
    render(<LocalUpgradeFlow daemon={daemon} copies={{ check, api }} runtime={deck} onClose={vi.fn()} />);
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
    render(<LocalUpgradeFlow daemon={daemon} copies={{ check: { ...check, app: manual }, api }} runtime={deck} onClose={vi.fn()} />);
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
    render(<LocalUpgradeFlow daemon={daemon} copies={{ check: waiting, api }} runtime={deck} onClose={onClose} />);
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

/**
 * Issue #2062 — the app's notice (the Dashboard banner's Upgrade… and the
 * rail's upgrade button) runs the Local daemon section's flow: the app, then
 * the CLI when it is behind, whatever its install method, then the local
 * daemon. Each CLI plan below is one the bridge reports for an install method
 * (`self_upgrade_040` on macOS and `self_upgrade_041` on Linux pin which
 * method gives which plan); here they drive the dialog the notice opens.
 */
describe("Upgrade from the app's notice covers the CLI too (issue #2062)", () => {
  const headline = (label: string) => `${label}: update available: v0.45.0 (current: v0.44.0)`;
  const appPlan: SelfUpgradePlan = {
    copy: "app",
    label: "Agent Deck (desktop app)",
    headline: headline("Agent Deck (desktop app)"),
    current: "0.44.0",
    latest: "0.45.0",
    action: "swap-app",
    actionable: true,
    confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.45.0?",
    provenance: { checked: true, reason: null },
    lines: [{ text: headline("Agent Deck (desktop app)"), command: null }],
  };
  const cliPlan = (action: SelfUpgradePlan["action"], actionable: boolean, how: SelfUpgradePlan["lines"]): SelfUpgradePlan => ({
    copy: "cli",
    label: "dot-agent-deck CLI",
    headline: headline("dot-agent-deck CLI"),
    current: "0.44.0",
    latest: "0.45.0",
    action,
    actionable,
    confirmQuestion: actionable ? "Upgrade dot-agent-deck CLI to v0.45.0?" : null,
    provenance: { checked: true, reason: null },
    lines: [{ text: headline("dot-agent-deck CLI"), command: null }, ...how],
  });
  const text = (value: string) => ({ text: value, command: null });
  const checkWith = (cli: SelfUpgradePlan | null, app: SelfUpgradePlan = appPlan): SelfUpgradeCheck => ({
    checkId: 7,
    latest: "0.45.0",
    updateAvailable: true,
    notice: app.headline,
    installed: null,
    app,
    cli,
    recheckAfterSecs: 21600,
  });
  const swapped: SelfUpgradeResult = { copy: "app", ok: true, installed: true, relaunch: true, lines: [text("Replaced /Applications/Agent Deck.app with v0.45.0. Quit and reopen Agent Deck to run it.")] };
  const daemon: UpgradeTarget = { deckId: "local", deckName: "Local daemon", kind: "local-upgrade", offer: { kind: "offered", from: "0.43.0", to: "0.44.0" } };
  const runtime = () => ({
    upgradeDaemon: vi.fn(async () => ({ outcome: "restarted", fromVersion: "0.43.0", toVersion: "0.44.0", stopped: { agents: [], roles: [] } }) as UpgradeOutcome),
    decideUpgrade: vi.fn(async () => undefined),
  });

  /* The CLI plans the app can carry out, with the result each reports. */
  const upgradable: { method: string; plan: SelfUpgradePlan; result: SelfUpgradeResult }[] = [
    {
      method: "Homebrew on macOS",
      plan: cliPlan("brew-upgrade", true, [text("Installed with Homebrew (/opt/homebrew/Cellar/dot-agent-deck/0.44.0/bin/dot-agent-deck). Upgrading runs `brew update`, then `brew upgrade dot-agent-deck`.")]),
      result: { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.")] },
    },
    {
      method: "Linuxbrew",
      plan: cliPlan("brew-upgrade", true, [text("Installed with Homebrew (/home/linuxbrew/.linuxbrew/Cellar/dot-agent-deck/0.44.0/bin/dot-agent-deck). Upgrading runs `brew update`, then `brew upgrade dot-agent-deck`.")]),
      result: { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.")] },
    },
    {
      method: "a downloaded binary in a writable directory",
      plan: cliPlan("replace-binary", true, [text("Downloaded binary at /home/u/.local/bin/dot-agent-deck. Upgrading replaces it in place.")]),
      result: { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("Upgraded /home/u/.local/bin/dot-agent-deck to v0.45.0.")] },
    },
    {
      method: "a downloaded binary in a directory you cannot write, on Linux",
      plan: cliPlan("staged-install", true, [text("/usr/local/bin is not writable by you. Upgrading downloads and checks it, then asks for your password to install it.")]),
      result: { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("Installed v0.45.0.")] },
    },
    {
      method: "a downloaded binary in a directory you cannot write, on macOS",
      plan: cliPlan("staged-install", true, [text("/usr/local/bin is not writable by you. The command to install it is shown once the download is verified.")]),
      result: { copy: "cli", ok: true, installed: false, relaunch: false, lines: [text("Downloaded and checked v0.45.0. Install it with:"), { text: "sudo install -m 0755 /Users/u/.local/state/dot-agent-deck/upgrade/dot-agent-deck /usr/local/bin/dot-agent-deck", command: "sudo install -m 0755 /Users/u/.local/state/dot-agent-deck/upgrade/dot-agent-deck /usr/local/bin/dot-agent-deck" }] },
    },
  ];

  for (const { method, plan: cli, result } of upgradable) {
    /** Scenario: The app and a CLI the app can upgrade are both behind. Opening the dialog from the app's notice shows both plans and says the CLI follows the app; Upgrade replaces the app, then the CLI is asked about and upgraded by its own install method, and the dialog ends at Relaunch, leaving the daemon to the relaunched app. */
    it(`notice_upgrade_001 upgrades the CLI after the app: ${method}`, async () => {
      const api = { check: vi.fn(), run: vi.fn(async (copy: string) => (copy === "app" ? swapped : result)), relaunch: vi.fn(async () => undefined) };
      const deck = runtime();
      const onClose = vi.fn();
      render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(cli), api }} runtime={deck} onClose={onClose} />);

      const plans = screen.getAllByTestId(/^self-upgrade-plan-/).map((section) => section.dataset.testid);
      expect(plans).toEqual(["self-upgrade-plan-app", "self-upgrade-plan-cli"]);
      expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent(CLI_AFTER_APP);
      expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "app");
      fireEvent.click(screen.getByTestId("self-upgrade-start"));
      await screen.findByTestId("self-upgrade-result-app");

      expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
      fireEvent.click(screen.getByTestId("self-upgrade-start"));
      expect(api.run).toHaveBeenLastCalledWith("cli", 7);
      await screen.findByTestId("self-upgrade-result-cli");
      expect(screen.getByTestId("self-upgrade-result-cli")).toHaveTextContent(result.lines[0].text);

      expect(screen.getByTestId("self-upgrade-relaunch")).toBeInTheDocument();
      fireEvent.click(screen.getByTestId("self-upgrade-close"));
      expect(onClose).toHaveBeenCalledTimes(1);
      expect(deck.upgradeDaemon).not.toHaveBeenCalled();
    });
  }

  /* The CLI plans the app cannot carry out: each says what to do instead. */
  const notUpgradable: { method: string; plan: SelfUpgradePlan; says: string; command?: string }[] = [
    { method: "Nix", plan: cliPlan("notify-only", false, [text("Installed with Nix (/nix/store/abc-dot-agent-deck-0.44.0/bin/dot-agent-deck), so it is not changed from here. Update your flake input (for example `nix flake update`) and rebuild, or run `nix profile upgrade`.")]), says: "nix profile upgrade" },
    { method: "built from source", plan: cliPlan("notify-only", false, [text("Built from source (it runs from a cargo build directory: /home/u/code/dot-agent-deck/target/release/dot-agent-deck), so it is not replaced from here. Check out v0.45.0 and rebuild.")]), says: "Check out v0.45.0 and rebuild" },
    { method: "owned by another system package", plan: cliPlan("notify-only", false, [text("Installed by the system package `dot-agent-deck-cli` (/usr/local/bin/dot-agent-deck), so it is not replaced from here. Upgrade that package with your package manager.")]), says: "Upgrade that package with your package manager" },
    { method: "Homebrew whose brew was not found", plan: cliPlan("show-command", false, [text("Installed with Homebrew (/opt/homebrew/Cellar/dot-agent-deck/0.44.0/bin/dot-agent-deck), but `brew` was not found. Upgrade it with:"), { text: "brew upgrade dot-agent-deck", command: "brew upgrade dot-agent-deck" }]), says: "Upgrade it with:", command: "brew upgrade dot-agent-deck" },
  ];

  for (const { method, plan: cli, says, command } of notUpgradable) {
    /** Scenario: The app is behind and the CLI beside it is one the app cannot upgrade. The dialog from the app's notice shows the CLI's plan, saying what to run or do, under the app's; only the app is asked about, and the CLI is never run. */
    it(`notice_upgrade_002 says what to do for a CLI the app cannot upgrade: ${method}`, async () => {
      const api = { check: vi.fn(), run: vi.fn(async () => swapped), relaunch: vi.fn(async () => undefined) };
      const deck = runtime();
      render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(cli), api }} runtime={deck} onClose={vi.fn()} />);

      expect(screen.getByTestId("self-upgrade-plan-cli")).toHaveTextContent(says);
      if (command) expect(screen.getByTestId("self-upgrade-plan-cli")).toContainElement(screen.getByTestId("self-upgrade-copy"));
      expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "app");
      fireEvent.click(screen.getByTestId("self-upgrade-start"));
      await screen.findByTestId("self-upgrade-result-app");
      expect(screen.queryByTestId("self-upgrade-confirm")).not.toBeInTheDocument();
      expect(api.run).toHaveBeenCalledTimes(1);
      expect(api.run).toHaveBeenCalledWith("app", 7);
      expect(screen.getByTestId("self-upgrade-relaunch")).toBeInTheDocument();
    });
  }

  /** Scenario: The app is behind and the only dot-agent-deck on the machine is the copy bundled in the app (the .dmg's or the .deb's), or the CLI beside it is current. The dialog from the app's notice shows the app alone, with no CLI step. */
  it("notice_upgrade_003 shows no CLI step when there is no CLI behind", () => {
    const current = { ...cliPlan("up-to-date", false, []), headline: "dot-agent-deck CLI is up to date (v0.45.0).", current: "0.45.0" };
    for (const cli of [null, current]) {
      const api = { check: vi.fn(), run: vi.fn(), relaunch: vi.fn() };
      const { unmount } = render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(cli), api }} runtime={runtime()} onClose={vi.fn()} />);
      const plans = screen.getAllByTestId(/^self-upgrade-plan-/).map((section) => section.dataset.testid);
      expect(plans).toEqual(["self-upgrade-plan-app"]);
      expect(screen.queryByTestId("self-upgrade-then")).not.toBeInTheDocument();
      unmount();
    }
  });

  /** Scenario: The app's upgrade from its notice fails, and the CLI is upgraded: with no relaunch waiting, closing the dialog goes on to restart the older local daemon, as the Local daemon section's Upgrade does. */
  it("notice_upgrade_004 an app that was not replaced goes on to the daemon after the CLI", async () => {
    const failed: SelfUpgradeResult = { copy: "app", ok: false, installed: false, relaunch: false, lines: [text("Cannot download Agent Deck.")] };
    const brewed: SelfUpgradeResult = { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.")] };
    const api = { check: vi.fn(), run: vi.fn(async (copy: string) => (copy === "app" ? failed : brewed)), relaunch: vi.fn() };
    const deck = runtime();
    render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(upgradable[0].plan), api }} runtime={deck} onClose={vi.fn()} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-app");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-cli");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith("local", expect.any(Function)));
  });

  /** Scenario: Only the app is behind (the CLI is current) and its upgrade fails: the dialog from the notice closes without restarting the daemon, since nothing on this machine's side changed. */
  it("notice_upgrade_005 the app alone is not followed by the daemon", async () => {
    const failed: SelfUpgradeResult = { copy: "app", ok: false, installed: false, relaunch: false, lines: [text("Cannot download Agent Deck.")] };
    const api = { check: vi.fn(), run: vi.fn(async () => failed), relaunch: vi.fn() };
    const deck = runtime();
    const onClose = vi.fn();
    render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(null), api }} runtime={deck} onClose={onClose} />);
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-app");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: The app is behind but cannot be upgraded from here (an app folder you cannot write) while the CLI is behind too: the notice's dialog still shows the app's plan saying what to do, offers the CLI, and says the daemon restart follows it. */
  it("notice_upgrade_006 an app this app cannot upgrade still shows its plan, and the CLI is offered", async () => {
    const manual: SelfUpgradePlan = { ...appPlan, action: "manual-download", actionable: false, confirmQuestion: null, lines: [...appPlan.lines, text("/Applications is not writable by you. Download the .dmg, open it, and drag Agent Deck.app into /Applications, replacing the old one.")] };
    const brewed: SelfUpgradeResult = { copy: "cli", ok: true, installed: true, relaunch: false, lines: [text("`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.")] };
    const api = { check: vi.fn(), run: vi.fn(async () => brewed), relaunch: vi.fn() };
    const deck = runtime();
    render(<LocalUpgradeFlow entry="notice" daemon={daemon} copies={{ check: checkWith(upgradable[0].plan, manual), api }} runtime={deck} onClose={vi.fn()} />);
    expect(screen.getByTestId("self-upgrade-plan-app")).toHaveTextContent("drag Agent Deck.app into /Applications");
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent("After the CLI, Agent Deck restarts the daemon on this machine");
    expect(screen.getByTestId("self-upgrade-confirm")).toHaveAttribute("data-copy", "cli");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-cli");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith("local", expect.any(Function)));
  });

  /** Scenario: For the same machine, the app's notice and the Local daemon section's Upgrade cover the same copies whenever the app can be upgraded from here; they differ only for an app that cannot, whose plan the notice still shows. */
  it("notice_upgrade_007 the notice and the Local daemon section cover the same copies", () => {
    const behind = checkWith(upgradable[0].plan);
    expect(upgradeCopies(behind, "notice")).toEqual(["app", "cli"]);
    expect(upgradeCopies(behind, "local")).toEqual(["app", "cli"]);
    const waiting = { ...behind, updateAvailable: false, installed: swapped };
    expect(upgradeCopies(waiting, "notice")).toEqual(["app", "cli"]);
    expect(upgradeCopies(waiting, "local")).toEqual(["app", "cli"]);
    const manual = checkWith(upgradable[0].plan, { ...appPlan, action: "manual-download", actionable: false, confirmQuestion: null });
    expect(upgradeCopies(manual, "notice")).toEqual(["app", "cli"]);
    expect(upgradeCopies(manual, "local")).toEqual(["cli"]);
    expect(upgradeCopies(checkWith(null), "notice")).toEqual(["app"]);
    expect(APP_BEFORE_CLI).not.toEqual(CLI_AFTER_APP);
    /* Shown whole: the dialog shortens a longer one. */
    for (const sentence of [APP_BEFORE_CLI, CLI_AFTER_APP]) expect(sentence.length).toBeLessThanOrEqual(DISPLAY_LIMITS.message);
  });
});
