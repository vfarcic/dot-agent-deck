import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import type { DeckActionResult, DeckDirectoryListing, DeckFleet, DeckRuntimeState, NewAgentOptions } from "../types";
import { AgentOverview } from "./AgentOverview";
import { DRAFT_RESTORED } from "./NewAgentDialog";

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

/** The only deck is chosen on open: use its home, and start the agent. */
async function startFromDialog() {
  fireEvent.keyDown(await screen.findByTestId("new-agent-directory-list"), { key: " " });
  await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));
  fireEvent.click(screen.getByTestId("new-agent-start"));
}

describe("the overview's New agent entry points (PRD #1223 M4)", () => {
  /**
   * Scenario: open the overview on the four-deck fleet and click the top bar's
   * New agent. The dialog opens with nothing preselected — two decks can take
   * a spawn — so no deck is chosen and focus is on the daemon field. A runtime
   * without the flow's queries renders no such control at all.
   */
  it("opens the flow from the top bar, and offers none without the flow's queries", () => {
    const current = runtime();
    render(<AgentOverview runtime={current} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    expect(dialog()).not.toBeNull();
    expect(highlightedDeck()).toBeUndefined();
    expect(screen.getByTestId("new-agent-deck-list")).toHaveFocus();
    expect(current.listDirectories).not.toHaveBeenCalled();
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
   * flow with that daemon chosen, and asks that daemon — and no other — for its
   * home.
   */
  it("opens the flow from a daemon group's header with that daemon preselected", async () => {
    const current = runtime();
    render(<AgentOverview runtime={current} onNavigate={vi.fn()} />);
    const headers = screen.getAllByTestId("daemon-new-agent");
    expect(headers).toHaveLength(2);

    const remoteGroup = screen.getAllByTestId("daemon-group").find((group) => group.getAttribute("data-daemon-id") === FIXTURE_REMOTE_DAEMON_ID)!;
    fireEvent.click(within(remoteGroup).getByTestId("daemon-new-agent"));

    expect(highlightedDeck()).toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(screen.getByTestId("new-agent-chosen-deck")).toBeVisible();
    // The first listing follows the daemon's options answer (PRD #1223's
    // `defaultDir`), so it is awaited rather than read synchronously.
    await waitFor(() => expect(current.listDirectories).toHaveBeenCalledTimes(1));
    expect(current.listDirectories).toHaveBeenCalledWith(FIXTURE_REMOTE_DAEMON_ID, undefined);
  });

  /**
   * Scenario (PRD #1223 U1): the remote deck does not advertise the listing
   * verb, so its connection carries `newAgentReason`. Its header offers no New
   * agent — the flow could not choose it — while the local deck's
   * header still does.
   */
  it("offers no header entry point on a daemon the flow cannot browse", () => {
    const fleet = createFixtureFleet("fleet").map((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID ? { ...deck, connection: { ...deck.connection, newAgentReason: "This deck does not advertise list-directories." } } : deck);
    render(<AgentOverview runtime={runtime({ fleet })} onNavigate={vi.fn()} />);
    const group = (deckId: string) => screen.getAllByTestId("daemon-group").find((candidate) => candidate.getAttribute("data-daemon-id") === deckId)!;
    expect(within(group(FIXTURE_REMOTE_DAEMON_ID)).queryByTestId("daemon-new-agent")).toBeNull();
    expect(within(group(FIXTURE_DAEMON_ID)).getByTestId("daemon-new-agent")).toBeVisible();
  });

  /**
   * Scenario: a healthy deck running nothing. Its first-run note offers New
   * agent instead of pointing at the CLI, and the flow opens on that daemon.
   */
  it("offers the flow from the first-run note", () => {
    render(<AgentOverview runtime={runtime({ fleet: [createFixtureSnapshot("empty")] })} onNavigate={vi.fn()} />);
    const note = screen.getByTestId("overview-first-run");
    expect(note).not.toHaveTextContent("from the CLI");

    fireEvent.click(within(note).getByTestId("overview-first-run-new-agent"));

    expect(highlightedDeck()).toBe(FIXTURE_DAEMON_ID);
  });

  /** Everything focusable that is neither inside the dialog nor under an `inert`. */
  function reachableOutside(flow: HTMLElement): Element[] {
    const candidates = document.querySelectorAll("button, a[href], input, select, textarea, [tabindex]");
    return Array.from(candidates).filter((element) => !flow.contains(element) && !element.closest("[inert]"));
  }

  /**
   * Scenario (Greptile's review of PR #1235): open the flow from the top bar
   * with that button focused. Every control of the overview behind it is inert
   * — so Tab cannot walk out into the daemon groups or the column picker the way
   * it could — focus is inside the dialog, and closing with Esc gives the
   * screen back and puts focus on the button that opened it.
   *
   * This tier can assert that the marking happens and that focus moves; what
   * the `inert` attribute DOES to a tab order is unobservable here, because
   * jsdom implements no focus semantics for it (see `useInertBackground`). That
   * half is pinned in `desktop/e2e/new-agent.spec.ts`, against a real engine.
   */
  it("fences the overview behind the flow and gives focus back to the opener", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const opener = screen.getByTestId("overview-new-agent");
    expect(opener.closest("[inert]")).toBeNull();
    // A real click focuses the button it presses; `fireEvent.click` does not,
    // and the opener is the thing under test here.
    opener.focus();
    fireEvent.click(opener);

    const flow = screen.getByTestId("new-agent-dialog");
    expect(opener.closest("[inert]")).not.toBeNull();
    // Equality to the empty set rather than a spot check: any control left
    // reachable behind a dialog that calls itself modal is the regression.
    expect(reachableOutside(flow)).toEqual([]);
    expect(flow.contains(document.activeElement)).toBe(true);

    fireEvent.keyDown(flow, { key: "Escape" });

    expect(dialog()).toBeNull();
    expect(document.querySelectorAll("[inert]")).toHaveLength(0);
    expect(document.activeElement).toBe(opener);
  });
});

describe("the overview after a New agent start (PRD #1223 M5)", () => {
  /**
   * Scenario: start an agent on the local deck, which answers with agent `9`.
   * Until the daemon's fleet entry lists `9`, nothing navigates; once it does,
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
   * Scenario: the daemon accepts the start and never lists the agent. After the
   * bound the dialog closes, no pane opens, and the overview says the agent
   * was started on that daemon and has not been listed; the line can be
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
      expect(notice).toHaveTextContent("Started dev on Local daemon, but the daemon has not listed it yet.");
      fireEvent.click(within(notice).getByRole("button", { name: "Dismiss" }));
      expect(screen.queryByTestId("overview-new-agent-notice")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("the overview keeps a closed New agent form (issue 1247)", () => {
  /** One healthy local deck, chosen on open, whose home has one child to choose. */
  function draftRuntime(overrides: Partial<DeckRuntimeState> = {}) {
    const HOME_WITH_CHILD: DeckDirectoryListing = { kind: "listing", path: "/home/dev", displayPath: "/home/dev", parent: "/home", entries: [{ path: "/home/dev/api", displayName: "api", isProject: false }], truncated: false };
    const API: DeckDirectoryListing = { kind: "listing", path: "/home/dev/api", displayPath: "/home/dev/api", parent: "/home/dev", entries: [], truncated: false };
    return runtime({
      fleet: [createFixtureSnapshot("empty")],
      listDirectories: vi.fn(async (_deckId: string, path?: string) => structuredClone(path === "/home/dev/api" ? API : HOME_WITH_CHILD)),
      ...overrides,
    });
  }

  /** Open the flow, choose `api`, and type a Name and a Command. */
  async function fillForm() {
    fireEvent.click(screen.getByTestId("overview-new-agent"));
    fireEvent.click(await screen.findByText("api"));
    fireEvent.click(await screen.findByTestId("new-agent-use-directory"));
    await waitFor(() => expect(screen.getByTestId("new-agent-name")).toBeEnabled());
    fireEvent.change(screen.getByTestId("new-agent-name"), { target: { value: "billing worker" } });
    fireEvent.change(screen.getByTestId("new-agent-command"), { target: { value: "claude --resume" } });
  }

  /**
   * Scenario: fill the New agent form, press Esc by accident, and open New
   * agent again. The form comes back — deck, directory, Name and the
   * hand-edited Command — with a notice saying it was restored, where it used
   * to open blank.
   */
  it("restores the form after Esc", async () => {
    const current = draftRuntime();
    render(<AgentOverview runtime={current} onNavigate={vi.fn()} />);
    await fillForm();

    fireEvent.keyDown(screen.getByTestId("new-agent-dialog"), { key: "Escape" });
    expect(dialog()).toBeNull();
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    await waitFor(() => expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("/home/dev/api"));
    expect(screen.getByTestId("new-agent-name")).toHaveValue("billing worker");
    expect(screen.getByTestId("new-agent-command")).toHaveValue("claude --resume");
    expect(screen.getByTestId("new-agent-restored")).toHaveTextContent(DRAFT_RESTORED);
  });

  /**
   * Scenario: fill the form, press Discard, and open New agent again. It is a
   * fresh form: no directory, the Name and Command back to their defaults,
   * and no restored notice.
   */
  it("forgets the form after Discard", async () => {
    render(<AgentOverview runtime={draftRuntime()} onNavigate={vi.fn()} />);
    await fillForm();

    fireEvent.click(screen.getByTestId("new-agent-discard"));
    expect(dialog()).toBeNull();
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));
    expect(screen.getByTestId("new-agent-dir")).toHaveTextContent("No directory chosen yet");
    expect(screen.getByTestId("new-agent-name")).toHaveValue("");
    expect(screen.queryByTestId("new-agent-restored")).toBeNull();
  });

  /**
   * Scenario: fill the form and start it; the deck accepts the start and
   * reports no agent to wait for. Opening New agent again gives a fresh form —
   * a started form is not a draft, and restoring it would invite starting it
   * twice.
   */
  it("forgets the form once the deck has accepted the start", async () => {
    const current = draftRuntime({ runAction: vi.fn(async (): Promise<DeckActionResult> => ({ ok: true })) });
    render(<AgentOverview runtime={current} onNavigate={vi.fn()} />);
    await fillForm();

    fireEvent.click(screen.getByTestId("new-agent-start"));
    await waitFor(() => expect(dialog()).toBeNull());
    fireEvent.click(screen.getByTestId("overview-new-agent"));

    await waitFor(() => expect(screen.getByTestId("new-agent-command")).toHaveValue("claude"));
    expect(screen.getByTestId("new-agent-name")).toHaveValue("");
    expect(screen.queryByTestId("new-agent-restored")).toBeNull();
  });
});
