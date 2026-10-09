import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { createFixtureFleet, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import type { UpgradeOutcome } from "../lib/upgrade";
import { VoiceOn } from "../hooks/useVoiceOn";
import type { DeckRuntimeState, DeckSnapshot } from "../types";
import { AgentOverview } from "./AgentOverview";

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

  /** Scenario: A runtime that cannot upgrade offers no button rather than one that does nothing. */
  it("offers no button when the runtime cannot upgrade", () => {
    render(<AgentOverview runtime={runtime(createFixtureFleet("upgrade"), { upgradeDaemon: undefined })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("daemon-upgrade")).not.toBeInTheDocument();
    expect(screen.queryByTestId("overview-upgrade")).not.toBeInTheDocument();
  });
});
