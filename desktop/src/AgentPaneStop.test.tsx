import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckActionResult, DeckRuntimeState, DeckSnapshot } from "./types";

/**
 * Issue #1402 — stopping the agent the open pane shows, from the pane itself.
 *
 * Before it, the pane's only header control was ✕ (Back to dashboard), which
 * closes the VIEW and leaves the agent running, and the inert fence over the
 * base screen put every other stop out of reach. These tests pin the control,
 * the confirmation it opens (the overview row's own, built by
 * `stopAgentConfirm`, including what it says for one role of an orchestration),
 * the `stop_agent` it sends, and the return: the view closes with the agent
 * once the deck stops listing it, so a confirmed stop never leaves an empty
 * agent screen behind.
 *
 * Both pane origins are covered — the overview's sibling pane and the deck's
 * promoted tile — because the shell owns one confirmation for the two.
 */
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>terminal</pre>
  ),
}));

import { DeckShell as AppDeckShell } from "./App";

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => {
      document = structuredClone(next);
      return structuredClone(document);
    }),
  };
}

function runtime(overrides: Partial<DeckRuntimeState> = {}, snapshot: DeckSnapshot = createFixtureSnapshot("connected")): DeckRuntimeState {
  const settings = settingsStore();
  return {
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    ...overrides,
  } as unknown as DeckRuntimeState;
}

/** The pane for `agentId`, opened over the overview. */
function overviewPane(agentId: string) {
  return { kind: "agent" as const, deckId: FIXTURE_DAEMON_ID, agentId, from: "overview" as const };
}

/** The pane's Close agent control, from whichever tile renders the pane. */
const paneStop = () => within(screen.getByTestId("agent-pane-overlay")).getByTestId("agent-pane-stop-agent");

describe("the agent screen's Close agent (issue #1402)", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  /**
   * Scenario: Planner's pane is open over the dashboard. Its header offers
   * Close agent beside the ✕, named and titled exactly as the dashboard row's
   * stop control. Pressed, it opens the row's own confirmation naming the
   * agent and the daemon it is on; Cancel sends nothing and keeps the pane.
   * Pressed again and confirmed, it sends one `stop_agent` to that daemon —
   * and once the daemon stops listing the agent, the pane closes with it and
   * the dashboard is what is on screen: no empty agent screen is left behind.
   */
  it("confirms, stops the agent on its daemon, and returns to the dashboard with it", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    const connected = createFixtureSnapshot("connected");
    const { rerender } = render(<AppDeckShell runtime={runtime({ runAction }, connected)} initialView={overviewPane("planner")} />);

    const stop = paneStop();
    expect(stop).toHaveAccessibleName("Close Plan / architecture agent");
    expect(stop).toHaveAttribute("title", "Close Plan / architecture");
    expect(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Back to dashboard" })).toBeVisible();

    fireEvent.click(stop);
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveTextContent("Close Plan / architecture?");
    expect(dialog).toHaveTextContent("This sends a stop request to Plan / architecture on Local daemon.");
    expect(dialog).not.toHaveTextContent("only this one role");
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(runAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();

    fireEvent.click(paneStop());
    await act(async () => fireEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Close agent" })));
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: undefined });
    // Nothing has ended yet: the pane stays until the deck stops listing the agent.
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();

    const stopped = { ...connected, agents: connected.agents.filter((agent) => agent.id !== "planner") };
    rerender(<AppDeckShell runtime={runtime({ runAction }, stopped)} initialView={overviewPane("planner")} />);
    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());
    expect(screen.getByTestId("overview-table-region")).toBeVisible();
  });

  /**
   * Scenario: the same pane opened from the deck screen, by enlarging a tile.
   * The control is there too, its confirmation is the same one, and a
   * confirmed stop returns to the DECK — the screen the pane was opened over —
   * not to the dashboard.
   */
  it("stops from the deck-origin pane and returns to the deck", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => ({ ok: true }));
    const connected = createFixtureSnapshot("connected");
    const deckView = { ...overviewPane("planner"), from: "deck" as const };
    const { rerender } = render(<AppDeckShell runtime={runtime({ runAction }, connected)} initialView={deckView} />);

    fireEvent.click(paneStop());
    await act(async () => fireEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Close agent" })));
    expect(runAction).toHaveBeenCalledWith({ type: "stop_agent", deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: undefined });

    const stopped = { ...connected, agents: connected.agents.filter((agent) => agent.id !== "planner") };
    rerender(<AppDeckShell runtime={runtime({ runAction }, stopped)} initialView={deckView} />);
    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());
    expect(screen.getByTestId("stop-run")).toBeVisible();
  });

  /**
   * Scenario: the pane shows one role of an orchestration. Closing it is a
   * different outcome from closing a standalone agent — the rest of the run
   * keeps going — so the confirmation says so, naming the run.
   */
  it("says what closing one role leaves running", () => {
    const crowded = createFixtureSnapshot("crowded");
    render(<AppDeckShell runtime={runtime({}, crowded)} initialView={overviewPane("2")} />);

    fireEvent.click(paneStop());
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveTextContent("Close coder?");
    expect(dialog).toHaveTextContent("This closes only this one role — the rest of the PRD 745 · agent dashboard orchestration keeps running.");
  });

  /**
   * Scenario: the pane's daemon stops answering while the pane is open. The
   * pane stays, explaining that — and offers no stop: nothing the app can
   * reach is running to stop, and the control would only send a request that
   * cannot be honoured into a silence the pane is already describing.
   */
  it("withholds the control while the pane's daemon is not answering", () => {
    const connected = createFixtureSnapshot("connected");
    const { rerender } = render(<AppDeckShell runtime={runtime({}, connected)} initialView={overviewPane("planner")} />);
    expect(paneStop()).toBeVisible();

    const silent: DeckSnapshot = {
      ...connected,
      connection: { ...connected.connection, status: "disconnected", message: "No daemon is listening on the configured socket." },
      agents: [],
    };
    rerender(<AppDeckShell runtime={runtime({}, silent)} initialView={overviewPane("planner")} />);
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(screen.getByTestId("terminal-absent-planner")).toBeVisible();
    expect(within(screen.getByTestId("agent-pane-overlay")).queryByTestId("agent-pane-stop-agent")).toBeNull();
  });

  /**
   * Scenario: the daemon refuses the stop. The confirmation still closes (the
   * runtime has filed the failure under its global error) and the pane stays:
   * the agent was not ended, so the view must not close either.
   */
  it("keeps the pane when the daemon refuses the stop", async () => {
    const runAction = vi.fn(async (): Promise<DeckActionResult> => { throw new Error("daemon returned error: no such agent"); });
    render(<AppDeckShell runtime={runtime({ runAction })} initialView={overviewPane("planner")} />);

    fireEvent.click(paneStop());
    await act(async () => fireEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "Close agent" })));
    expect(runAction).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
  });
});
