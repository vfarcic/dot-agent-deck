import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { createFixtureFleet, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import type { UpgradeOutcome } from "../lib/upgrade";
import { VoiceOn } from "../hooks/useVoiceOn";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { AgentOverview } from "./AgentOverview";
import { SelfUpgradeDialog } from "./SelfUpgradeDialog";
import type { SelfUpgradeCheck, SelfUpgradePlan, SelfUpgradeResult } from "../lib/selfUpgrade";
import { CLI_LEAVES_DAEMON } from "./LocalUpgradeFlow";

/**
 * PRD #1487 M5 — Upgrade on a deck's card on the agent dashboard (D9). The
 * `upgrade` fixture fleet is: this machine's deck at the app's own release,
 * a connected remote deck on an older release with agents running, and a
 * refused remote deck on an older release.
 */
function runtime(fleet: DeckSnapshot[], overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  return {
    mode: "fixture",
    snapshot: fleet[0],
    fleet,
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    upgradeDaemon: vi.fn(async () => ({ outcome: "restarted", fromVersion: "0.44.0", toVersion: "0.45.0", stopped: { agents: [], roles: [] } }) as UpgradeOutcome),
    decideUpgrade: vi.fn(async () => undefined),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    testEndpoint: vi.fn(),
    secretStatus: vi.fn(),
    storeSecret: vi.fn(),
    forgetSecret: vi.fn(),
    getSettings: vi.fn(),
    saveSettings: vi.fn(),
    ...overrides,
  } as unknown as DeckRuntimeState;
}

/** A deck's card, by the name its header shows: "Local daemon" for this machine's, the address for a remote one. */
const group = (deckId: string) => {
  const name = deckId === FIXTURE_DAEMON_ID ? "Local daemon" : deckId;
  const found = screen.getAllByTestId("daemon-group").find((section) => within(section).getByTestId("daemon-identity").textContent === name);
  if (!found) throw new Error(`no card for ${name}`);
  return found;
};

describe("the dashboard's Upgrade action", () => {
  /** Scenario: Shows Upgrade on an older remote deck's card header and nowhere for a deck at the app's release. */
  it("offers Upgrade on the card of an older remote deck only", () => {
    render(<AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={vi.fn()} />);

    expect(within(group(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-upgrade")).toBeVisible();
    expect(within(group(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-upgrade")).toHaveAccessibleName(`Upgrade the daemon on ${FIXTURE_REMOTE_DAEMON_ID}`);
    expect(within(group(FIXTURE_DAEMON_ID)).queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
    // The refused deck carries it in its note instead, with the sentence that explains it — never twice.
    const refused = group(FIXTURE_UNREACHABLE_DAEMON_ID);
    expect(within(refused).queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
    expect(within(refused).getByTestId("overview-upgrade")).toBeVisible();
    expect(within(refused).getByTestId("overview-incompatible")).toHaveTextContent("Upgrade installs this app's version on that machine");
  });

  /** Scenario: A plain fleet with no older daemon shows no Upgrade anywhere. */
  it("offers nothing when no daemon is older", () => {
    render(<AgentOverview runtime={runtime(createFixtureFleet("fleet"))} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
    expect(screen.queryByTestId("overview-upgrade")).not.toBeInTheDocument();
  });

  /** Scenario: Upgrade on a card opens the dialog for THAT deck and runs it there. */
  it("upgrades the deck whose card it was pressed on", async () => {
    const deck = runtime(createFixtureFleet("upgrade"));
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} />);

    fireEvent.click(within(group(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-upgrade"));
    expect(screen.getByRole("alertdialog")).toHaveTextContent(`Upgrade the daemon on ${FIXTURE_REMOTE_DAEMON_ID}?`);
    expect(screen.getByTestId("upgrade-confirm-body")).toHaveTextContent("This installs 0.45.0");
    fireEvent.click(screen.getByTestId("upgrade-start"));
    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith(FIXTURE_REMOTE_DAEMON_ID, expect.any(Function)));
    expect(await screen.findByTestId("upgrade-outcome")).toHaveTextContent("now runs 0.45.0");
    fireEvent.click(screen.getByTestId("upgrade-close"));
    expect(screen.queryByTestId("upgrade-dialog")).not.toBeInTheDocument();
  });

  /** Scenario: With voice on, a row's number opens its agent; with the Upgrade dialog up, the same digit opens nothing behind it. */
  it("keeps the dashboard's number keys off while the Upgrade dialog is open", () => {
    const onNavigate = vi.fn();
    render(<VoiceOn.Provider value={true}><AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={onNavigate} /></VoiceOn.Provider>);

    fireEvent.click(within(group(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-upgrade"));
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "1" });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(screen.getByTestId("upgrade-dialog")).toBeInTheDocument();

    // The control: the same digit with the dialog closed opens the first row.
    fireEvent.click(screen.getByTestId("upgrade-cancel"));
    expect(screen.queryByTestId("upgrade-dialog")).not.toBeInTheDocument();
    fireEvent.keyDown(document.body, { key: "1" });
    expect(onNavigate).toHaveBeenCalled();
  });

  /** Scenario: With voice on, a modal this screen did not open (the shell's own Upgrade dialog) also keeps a digit from opening a row behind it. */
  it("keeps the dashboard's number keys off under a modal it does not own", () => {
    const onNavigate = vi.fn();
    const view = render(<VoiceOn.Provider value={true}><AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={onNavigate} /><div role="alertdialog" aria-modal="true" data-testid="shell-modal" /></VoiceOn.Provider>);
    fireEvent.keyDown(document.body, { key: "1" });
    expect(onNavigate).not.toHaveBeenCalled();

    // The control: the same digit once that modal is gone opens the first row.
    view.rerender(<VoiceOn.Provider value={true}><AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={onNavigate} /></VoiceOn.Provider>);
    fireEvent.keyDown(document.body, { key: "1" });
    expect(onNavigate).toHaveBeenCalled();
  });

  /** Scenario: A runtime that cannot upgrade offers no button rather than one that does nothing. */
  it("offers no button when the runtime cannot upgrade", () => {
    render(<AgentOverview runtime={runtime(createFixtureFleet("upgrade"), { upgradeDaemon: undefined })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
    expect(screen.queryByTestId("overview-upgrade")).not.toBeInTheDocument();
  });
});

describe("the newer-release dialog over the dashboard", () => {
  const headline = "Agent Deck (desktop app): update available: v0.47.0 (current: v0.46.0)";
  const check: SelfUpgradeCheck = {
    checkId: 1,
    latest: "0.47.0",
    updateAvailable: true,
    notice: headline,
    installed: null,
    app: { copy: "app", label: "Agent Deck (desktop app)", headline, current: "0.46.0", latest: "0.47.0", action: "swap-app", actionable: true, confirmQuestion: "Upgrade Agent Deck (desktop app) to v0.47.0?", provenance: { checked: true, reason: null }, lines: [{ text: headline, command: null }] },
    cli: null,
    recheckAfterSecs: 21600,
  };
  const api = { check: vi.fn(), run: vi.fn(), relaunch: vi.fn() };

  /** Scenario: With voice on, the dashboard's rows are numbered. While the app's newer-release dialog is open over it — with Cancel or Upgrade focused — pressing a row's number opens nothing behind the dialog; once it is closed, the same digit opens the row. */
  it("self_upgrade_modal_001 keeps the dashboard's number keys off while the dialog is open", () => {
    const onNavigate = vi.fn();
    const { rerender } = render(
      <VoiceOn.Provider value={true}>
        <AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={onNavigate} />
        <SelfUpgradeDialog check={check} api={api} copies={["app"]} onClose={vi.fn()} />
      </VoiceOn.Provider>,
    );
    expect(screen.getByTestId("self-upgrade-cancel")).toHaveFocus();
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "1" });
    screen.getByTestId("self-upgrade-start").focus();
    fireEvent.keyDown(document.activeElement ?? document.body, { key: "2" });
    expect(onNavigate).not.toHaveBeenCalled();
    expect(api.run).not.toHaveBeenCalled();

    // The control: the same digit with the dialog closed opens the first row.
    rerender(
      <VoiceOn.Provider value={true}>
        <AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={onNavigate} />
      </VoiceOn.Provider>,
    );
    fireEvent.keyDown(document.body, { key: "1" });
    expect(onNavigate).toHaveBeenCalled();
  });
});

describe("Upgrade on the Local daemon section (issue #1753)", () => {
  const cliHeadline = "dot-agent-deck CLI: update available: v0.45.0 (current: v0.44.0)";
  const cliPlan: SelfUpgradePlan = {
    copy: "cli",
    label: "dot-agent-deck CLI",
    headline: cliHeadline,
    current: "0.44.0",
    latest: "0.45.0",
    action: "brew-upgrade",
    actionable: true,
    confirmQuestion: "Upgrade dot-agent-deck CLI to v0.45.0?",
    provenance: { checked: true, reason: null },
    lines: [
      { text: cliHeadline, command: null },
      { text: "Installed with Homebrew (/opt/homebrew/Cellar/dot-agent-deck/0.44.0/bin/dot-agent-deck). Upgrading runs `brew upgrade dot-agent-deck`, which installs the formula's latest release.", command: null },
    ],
  };
  const appHeadline = "Agent Deck (desktop app) is up to date (v0.45.0).";
  /* The app is current, so the app-wide notice is empty; only the CLI is behind. */
  const check = (cli: SelfUpgradePlan | null = cliPlan): SelfUpgradeCheck => ({
    checkId: 9,
    latest: "0.45.0",
    updateAvailable: false,
    notice: null,
    installed: null,
    app: { copy: "app", label: "Agent Deck (desktop app)", headline: appHeadline, current: "0.45.0", latest: "0.45.0", action: "up-to-date", actionable: false, confirmQuestion: null, provenance: { checked: true, reason: null }, lines: [{ text: appHeadline, command: null }] },
    cli,
    recheckAfterSecs: 21600,
  });
  const brewed: SelfUpgradeResult = { copy: "cli", ok: true, relaunch: false, lines: [{ text: "`brew upgrade dot-agent-deck` finished; it now reports v0.45.0.", command: null }] };
  const selfApi = () => ({ check: vi.fn(), run: vi.fn(async () => brewed), relaunch: vi.fn() });
  /* The `upgrade` fleet with this machine's daemon on an older release than the app. */
  const olderLocal = () => createFixtureFleet("upgrade").map((deck) => (
    deck.connection.deckKind === "local" ? { ...deck, connection: { ...deck.connection, upgradeOffer: { kind: "offered" as const, from: "0.44.0", to: "0.45.0" } } } : deck
  ));

  /** Scenario: The app is current and the CLI is behind: the Local daemon section offers Upgrade, named for the CLI, with the CLI's notice on hover; pressing it shows the CLI's plan, saying the daemon is not restarted, and Upgrade upgrades the CLI alone — the daemon is never asked to restart. */
  it("local_upgrade_001 offers the CLI on the Local daemon section and leaves the daemon alone", async () => {
    const deck = runtime(createFixtureFleet("upgrade"));
    const api = selfApi();
    const onClosed = vi.fn();
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} selfUpgrade={{ check: check(), api, onClosed }} />);

    const button = within(group(FIXTURE_DAEMON_ID)).getByTestId("daemon-upgrade");
    expect(button).toHaveAccessibleName("Upgrade the CLI on this machine");
    expect(button).toHaveAttribute("title", cliHeadline);
    // A remote deck's Upgrade is still about its own daemon.
    expect(within(group(FIXTURE_REMOTE_DAEMON_ID)).getByTestId("daemon-upgrade")).toHaveAccessibleName(`Upgrade the daemon on ${FIXTURE_REMOTE_DAEMON_ID}`);

    fireEvent.click(button);
    expect(screen.getByTestId("self-upgrade-plan-cli")).toHaveTextContent(cliHeadline);
    expect(screen.queryByTestId("self-upgrade-plan-app")).not.toBeInTheDocument();
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent(CLI_LEAVES_DAEMON);
    expect(screen.getByTestId("self-upgrade-cancel")).toHaveFocus();
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    expect(api.run).toHaveBeenCalledWith("cli", 9);
    expect(await screen.findByTestId("self-upgrade-result-cli")).toHaveTextContent("it now reports v0.45.0");
    fireEvent.click(screen.getByTestId("self-upgrade-close"));

    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
    expect(onClosed).toHaveBeenCalledTimes(1);
  });

  /** Scenario: Both the CLI and the local daemon are behind: one Upgrade covers both. The CLI's dialog says the daemon restart follows; once the CLI is upgraded and the dialog closed, the daemon's restart onto the app's build starts on its own, through the daemon's usual question. */
  it("local_upgrade_002 upgrades the CLI, then restarts the older local daemon", async () => {
    const deck = runtime(olderLocal());
    const api = selfApi();
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} selfUpgrade={{ check: check(), api }} />);

    const button = within(group(FIXTURE_DAEMON_ID)).getByTestId("daemon-upgrade");
    expect(button).toHaveAccessibleName("Upgrade the CLI and the daemon on this machine");
    expect(button).toHaveAttribute("title", `${cliHeadline} Its daemon runs 0.44.0; this app is 0.45.0.`);
    fireEvent.click(button);
    expect(screen.getByTestId("self-upgrade-then")).toHaveTextContent("After the CLI, Agent Deck restarts the daemon on this machine, which runs 0.44.0, onto 0.45.0, the version that came with this app.");
    fireEvent.click(screen.getByTestId("self-upgrade-start"));
    await screen.findByTestId("self-upgrade-result-cli");
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));

    await waitFor(() => expect(deck.upgradeDaemon).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, expect.any(Function)));
    expect(await screen.findByTestId("upgrade-outcome")).toHaveTextContent("The daemon on this machine now runs 0.45.0");
  });

  /** Scenario: Both are behind and the user cancels the CLI's question: the whole Upgrade ends having changed nothing — no CLI upgrade and no daemon restart. */
  it("local_upgrade_003 Cancel on the CLI ends the whole upgrade", () => {
    const deck = runtime(olderLocal());
    const api = selfApi();
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} selfUpgrade={{ check: check(), api }} />);
    fireEvent.click(within(group(FIXTURE_DAEMON_ID)).getByTestId("daemon-upgrade"));
    fireEvent.click(screen.getByTestId("self-upgrade-cancel"));
    expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
    expect(api.run).not.toHaveBeenCalled();
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: The CLI is behind but cannot be upgraded from the app (Nix), and the daemon is older: the CLI's plan says what to do and offers only Close, after which the daemon's Upgrade asks first rather than starting on its own. */
  it("local_upgrade_004 asks before the daemon restart when the CLI was not upgraded", () => {
    const deck = runtime(olderLocal());
    const nix: SelfUpgradePlan = { ...cliPlan, action: "notify-only", actionable: false, confirmQuestion: null, lines: [cliPlan.lines[0], { text: "Installed with Nix (/nix/store/abc/bin/dot-agent-deck), so it is not changed from here.", command: null }] };
    render(<AgentOverview runtime={deck} onNavigate={vi.fn()} selfUpgrade={{ check: check(nix), api: selfApi() }} />);
    fireEvent.click(within(group(FIXTURE_DAEMON_ID)).getByTestId("daemon-upgrade"));
    expect(screen.queryByTestId("self-upgrade-start")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("self-upgrade-close"));
    expect(screen.getByRole("alertdialog")).toHaveTextContent("Upgrade the daemon on this machine?");
    expect(screen.getByTestId("upgrade-dialog")).toHaveAttribute("data-phase", "confirm");
    expect(deck.upgradeDaemon).not.toHaveBeenCalled();
  });

  /** Scenario: The CLI is current and the local daemon is at the app's release: the Local daemon section offers no Upgrade. */
  it("local_upgrade_005 offers nothing on the Local daemon section while both are current", () => {
    const current: SelfUpgradePlan = { ...cliPlan, action: "up-to-date", actionable: false, confirmQuestion: null, headline: "dot-agent-deck CLI is up to date (v0.45.0)." };
    render(<AgentOverview runtime={runtime(createFixtureFleet("upgrade"))} onNavigate={vi.fn()} selfUpgrade={{ check: check(current), api: selfApi() }} />);
    expect(within(group(FIXTURE_DAEMON_ID)).queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
  });
});
