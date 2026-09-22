import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import type { DeckActionResult, DeckDirectoryListing, DeckFleet, DeckRuntimeState, NewAgentOptions } from "../types";
import { AgentOverview } from "./AgentOverview";

const HOME: DeckDirectoryListing = { kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [], truncated: false };
const OPTIONS: NewAgentOptions = { kind: "deck", agents: [], experimental: false, authoringKinds: [], lastCommand: "claude" };

/**
 * A runtime carrying the New agent flow's two queries. `runtime.fleet` is the
 * fleet the overview renders and the dialog's deck step lists.
 */
function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const fleet = overrides.fleet ?? createFixtureFleet("fleet");
  return {
    mode: "fixture",
    snapshot: fleet[0],
    fleet,
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async (): Promise<DeckActionResult> => ({ ok: true, agentId: "9" })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    listDirectories: vi.fn(async () => structuredClone(HOME)),
    newAgentOptions: vi.fn(async () => structuredClone(OPTIONS)),
    getSettings: vi.fn(),
    saveSettings: vi.fn(),
    testEndpoint: vi.fn(),
    secretStatus: vi.fn(),
    storeSecret: vi.fn(),
    forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level),
    ...overrides,
  } as DeckRuntimeState;
}

const dialog = () => screen.queryByTestId("new-agent-dialog");
const highlightedDeck = () => screen.getByTestId("new-agent-deck-list").querySelector("[aria-selected='true']")?.getAttribute("data-deck-id");

/** Choose the highlighted deck, use its home, and start the agent. */
async function startFromDialog() {
  fireEvent.keyDown(screen.getByTestId("new-agent-deck-list"), { key: "Enter" });
  fireEvent.keyDown(await screen.findByTestId("new-agent-directory-list"), { key: " " });
  await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));
  fireEvent.click(screen.getByTestId("new-agent-start"));
}

describe("the overview's New agent entry points (PRD #1223 M4)", () => {
  /**
   * Scenario: open the overview on the four-deck fleet and click the top bar's
   * New agent. The dialog opens on its deck step with nothing preselected —
   * two decks can take a spawn. A runtime without the flow's queries renders
   * no such control at all.
   */
  it("opens the flow from the top bar, and offers none without the flow's queries", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    expect(dialog()).toHaveAttribute("data-step", "deck");
    expect(highlightedDeck()).toBeUndefined();
  });

  it("offers no New agent control for a runtime without the flow's queries", () => {
    render(<AgentOverview runtime={runtime({ listDirectories: undefined, newAgentOptions: undefined })} onNavigate={vi.fn()} />);

    expect(screen.queryByTestId("overview-new-agent")).toBeNull();
    expect(screen.queryByTestId("daemon-new-agent")).toBeNull();
    fireEvent.keyDown(window, { key: "n", ctrlKey: true });
    expect(dialog()).toBeNull();
  });

  /**
   * Scenario: press Ctrl+N on the overview, close the dialog, press Cmd+N. Each
   * opens the flow. Pressed inside a text field, or with an agent's pane open
   * over the screen, it opens nothing — the key belongs to what has focus.
   */
  it("opens the flow on Ctrl+N and Cmd+N, and not from a text field or under an open pane", () => {
    const { rerender } = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const ctrl = new KeyboardEvent("keydown", { key: "n", ctrlKey: true, bubbles: true, cancelable: true });
    act(() => { window.dispatchEvent(ctrl); });
    expect(dialog()).not.toBeNull();
    expect(ctrl.defaultPrevented).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Close new agent" }));
    expect(dialog()).toBeNull();

    fireEvent.keyDown(window, { key: "n", metaKey: true });
    expect(dialog()).not.toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Close new agent" }));

    const field = document.createElement("input");
    document.body.appendChild(field);
    fireEvent.keyDown(field, { key: "n", ctrlKey: true });
    expect(dialog()).toBeNull();
    field.remove();

    rerender(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} agentPaneOpen />);
    fireEvent.keyDown(window, { key: "n", ctrlKey: true });
    expect(dialog()).toBeNull();
  });

  /**
   * Scenario: each connected deck's group header carries its own New agent;
   * the unreachable and pending decks' do not. The remote deck's opens the
   * flow with that deck preselected.
   */
  it("opens the flow from a deck group's header with that deck preselected", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const headers = screen.getAllByTestId("daemon-new-agent");
    expect(headers).toHaveLength(2);

    const remoteGroup = screen.getAllByTestId("daemon-group").find((group) => group.getAttribute("data-daemon-id") === FIXTURE_REMOTE_DAEMON_ID)!;
    fireEvent.click(within(remoteGroup).getByTestId("daemon-new-agent"));

    expect(highlightedDeck()).toBe(FIXTURE_REMOTE_DAEMON_ID);
  });

  /**
   * Scenario: a healthy deck running nothing. Its first-run note offers New
   * agent instead of pointing at the CLI, and the flow opens on that deck.
   */
  it("offers the flow from the first-run note", () => {
    render(<AgentOverview runtime={runtime({ fleet: [createFixtureSnapshot("empty")] })} onNavigate={vi.fn()} />);
    const note = screen.getByTestId("overview-first-run");
    expect(note).not.toHaveTextContent("from the CLI");

    fireEvent.click(within(note).getByTestId("overview-first-run-new-agent"));

    expect(highlightedDeck()).toBe(FIXTURE_DAEMON_ID);
  });
});

describe("the overview after a New agent start (PRD #1223 M5)", () => {
  /**
   * Scenario: start an agent on the local deck, which answers with agent `9`.
   * Until the deck's fleet entry lists `9`, nothing navigates; once it does,
   * the dialog closes and the overview opens that agent's pane by its
   * composite identity, from the overview.
   */
  it("opens the new agent's pane once its deck lists it", async () => {
    const onNavigate = vi.fn();
    const first = runtime({ fleet: [createFixtureSnapshot("empty")] });
    const { rerender } = render(<AgentOverview runtime={first} onNavigate={onNavigate} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    await startFromDialog();
    expect(await screen.findByTestId("new-agent-waiting")).toBeVisible();
    expect(onNavigate).not.toHaveBeenCalled();

    const listed: DeckFleet = [{ ...first.fleet[0], agents: [createFixtureStartedAgent({ id: "9", daemonId: FIXTURE_DAEMON_ID, cwd: "/home/dev" })] }];
    rerender(<AgentOverview runtime={{ ...first, fleet: listed, snapshot: listed[0] }} onNavigate={onNavigate} />);

    await waitFor(() => expect(onNavigate).toHaveBeenCalledWith({ kind: "agent", deckId: FIXTURE_DAEMON_ID, agentId: "9", from: "overview" }));
    expect(dialog()).toBeNull();
    expect(first.runAction).toHaveBeenCalledWith({ type: "start_agent", deckId: FIXTURE_DAEMON_ID, cwd: "/home/dev", command: "claude", displayName: "dev" });
  });

  /**
   * Scenario: the deck accepts the start and never lists the agent. After the
   * bound the dialog closes, no pane opens, and the overview says the agent
   * was started on that deck and has not been listed; the line can be
   * dismissed.
   */
  it("says so on the overview when the agent has not appeared within the bound", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const onNavigate = vi.fn();
      render(<AgentOverview runtime={runtime({ fleet: [createFixtureSnapshot("empty")] })} onNavigate={onNavigate} />);
      fireEvent.click(screen.getByTestId("overview-new-agent"));
      await startFromDialog();
      await screen.findByTestId("new-agent-waiting");

      await act(async () => { await vi.advanceTimersByTimeAsync(12_000); });

      expect(dialog()).toBeNull();
      expect(onNavigate).not.toHaveBeenCalled();
      const notice = screen.getByTestId("overview-new-agent-notice");
      expect(notice).toHaveTextContent("Started dev on Local deck, but the deck has not listed it yet.");
      fireEvent.click(within(notice).getByRole("button", { name: "Dismiss" }));
      expect(screen.queryByTestId("overview-new-agent-notice")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});
