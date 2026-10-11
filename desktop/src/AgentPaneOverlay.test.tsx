import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentTileProps } from "./components/AgentTile";
import { OVERVIEW_CLOCK_TICK_MS } from "./components/AgentOverview";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, mapDesktopSnapshot, type DesktopSettingsDto } from "./lib/bridge";
import type { AgentTarget, DeckActionResult, DeckRuntimeState } from "./types";

/**
 * Two spies, because the property PRD #1105 M3 needs is about BUILDS and the
 * DOM can only speak about what is on screen now.
 *
 * `terminalBuilt` fires from a mount effect and `terminalDisposed` from its
 * cleanup, so a tile that unmounted its viewport into an overlay and mounted a
 * fresh one there — which renders an identical-looking element and reads
 * identically to every DOM query — is a build and a dispose here. That is the
 * difference between promoting the tile's element in place and re-parenting it,
 * and it is what the real `TerminalViewport` pays for in a rebuilt xterm, a
 * second WebGL context and a client-side transcript re-write.
 */
const terminalBuilt = vi.fn();
const terminalDisposed = vi.fn();
const agentTileMounted = vi.fn();
const agentTileDisposed = vi.fn();
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => {
    useEffect(() => {
      terminalBuilt(agentId);
      return () => terminalDisposed(agentId);
    }, [agentId]);
    return <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>terminal</pre>;
  },
}));
vi.mock("./components/AgentTile", async () => {
  const actual = await vi.importActual<typeof import("./components/AgentTile")>("./components/AgentTile");
  const RealAgentTile = actual.AgentTile;
  return {
    ...actual,
    AgentTile: (props: AgentTileProps) => {
      useEffect(() => {
        agentTileMounted(props.agent.id);
        return () => agentTileDisposed(props.agent.id);
      }, [props.agent.id]);
      return <RealAgentTile {...props} />;
    },
  };
});

import { DeckShell as AppDeckShell } from "./App";

/** Existing deck-specific cases enter the deck explicitly after the app default changes. */
function DeckShell(props: Parameters<typeof AppDeckShell>[0]) {
  return <AppDeckShell initialView={{ kind: "deck" }} {...props} />;
}

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

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const snapshot = createFixtureSnapshot("connected");
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

/** How many times one agent's viewport has been built, and disposed. */
const buildsFor = (agentId: string) => terminalBuilt.mock.calls.filter(([id]) => id === agentId).length;
const disposesFor = (agentId: string) => terminalDisposed.mock.calls.filter(([id]) => id === agentId).length;

/**
 * PRD #1105 M3. The required property is exactly one live `TerminalViewport`
 * per agent at any moment, and it is a correctness requirement rather than a
 * preference: `terminalRegistry` is a module-level `Map` keyed by BARE agent
 * id, so a second viewport's `registerTerminal` overwrites the first's entry
 * and the first's unmount then cleans up nothing — the Reader would snapshot
 * whichever mounted last, and `registerRefit` has the same shape, so a zoom
 * would re-fit only one of the two. Two WebGL contexts and two
 * `ResizeObserver`s reporting different geometries into one per-agent coalesced
 * resize entry are the costs the PRD already names.
 */
describe("agent pane overlay", () => {
  beforeEach(() => {
    terminalBuilt.mockClear();
    terminalDisposed.mockClear();
    agentTileMounted.mockClear();
    agentTileDisposed.mockClear();
    window.localStorage.clear();
  });

  /**
   * Scenario: start directly in Planner's pane once from each origin. The same
   * real AgentTile component mounts at overlay presentation in both paths, so
   * a parallel large-pane component cannot impersonate it with matching DOM.
   */
  it("renders the same AgentTile component from the deck and overview origins", () => {
    const deckView = { kind: "agent" as const, deckId: FIXTURE_DAEMON_ID, agentId: "planner", from: "deck" as const };
    const deck = render(<DeckShell runtime={runtime()} initialView={deckView} />);

    expect(screen.getByTestId("agent-tile-planner")).toHaveAttribute("data-presentation", "overlay");
    expect(agentTileMounted.mock.calls.filter(([id]) => id === "planner")).toHaveLength(1);
    deck.unmount();
    agentTileMounted.mockClear();
    agentTileDisposed.mockClear();

    const overviewView = { ...deckView, from: "overview" as const };
    render(<DeckShell runtime={runtime()} initialView={overviewView} />);

    expect(screen.getByTestId("agent-tile-planner")).toHaveAttribute("data-presentation", "overlay");
    expect(agentTileMounted).toHaveBeenCalledTimes(1);
    expect(agentTileMounted).toHaveBeenCalledWith("planner");
    expect(agentTileDisposed).not.toHaveBeenCalled();
  });

  /**
   * Scenario: open Planner's pane from the deck and close it again. Planner's
   * terminal is built once for the whole round trip and is never disposed, and
   * the element on screen is the same DOM node throughout.
   */
  it("promotes the deck tile in place, so the agent keeps one terminal across open and close", () => {
    render(<DeckShell runtime={runtime()} />);
    const before = screen.getByTestId("terminal-planner");
    expect(buildsFor("planner")).toBe(1);

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(screen.getAllByTestId("terminal-planner")).toHaveLength(1);
    expect(screen.getByTestId("terminal-planner")).toBe(before);
    expect(buildsFor("planner")).toBe(1);
    expect(disposesFor("planner")).toBe(0);

    fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("terminal-planner")).toBe(before);
    expect(buildsFor("planner")).toBe(1);
    expect(disposesFor("planner")).toBe(0);
    // And the deck underneath is untouched: four agents, four viewports, one
    // each, none of them rebuilt by the pane opening over them. The lookahead
    // excludes `terminal-input-status-*`, which the tile renders beside the
    // viewport and which this count would otherwise double.
    expect(screen.getAllByTestId(/^terminal-(?!input-status)/)).toHaveLength(4);
    expect(terminalBuilt).toHaveBeenCalledTimes(4);
    expect(terminalDisposed).not.toHaveBeenCalled();
  });

  /**
   * Scenario: open the Reader on Planner's tile, then enlarge that same agent.
   * The pane renders neither the Reader launcher nor the Reader itself — the
   * launcher alone is not enough, because `readerOpen` is tile-local state that
   * only `agent.id` resets and it rides across the promotion.
   */
  it("carries no Output Reader into the pane, launcher or panel", () => {
    render(<DeckShell runtime={runtime()} />);
    fireEvent.click(screen.getByTestId("reader-open-planner"));
    expect(screen.getByTestId("reader-planner")).toBeVisible();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    const overlay = screen.getByTestId("agent-pane-overlay");
    // Both halves, because hiding the button while still rendering the panel
    // would leave two `window` `keydown` listeners answering one `Escape`, and
    // `stopPropagation` does nothing between listeners on the same target.
    expect(within(overlay).queryByTestId("reader-open-planner")).not.toBeInTheDocument();
    expect(screen.queryByTestId("reader-planner")).not.toBeInTheDocument();
  });

  /**
   * Scenario: open the Reader on Planner's tile, then open the pane on a
   * DIFFERENT agent, and press `Escape` once. Planner's Reader is already gone
   * by the time the key is pressed, so the one `Escape` reaches the pane and
   * nothing else.
   *
   * The test above covers the same agent being enlarged, which the
   * `presentation` prop settles on its own. This is the case it cannot see: the
   * tiles the pane is drawn OVER stay at `presentation="tile"`, so a background
   * Reader kept its own `window` `keydown` listener behind the scrim and both
   * answered the same key — closing the pane and a Reader the user could not
   * see, which falsified the "exactly one listener" claim in `DeckShell` and in
   * `AgentTile`.
   */
  it("dismisses a Reader open on another tile when a pane opens over it", () => {
    render(<DeckShell runtime={runtime()} />);
    fireEvent.click(screen.getByTestId("reader-open-planner"));
    expect(screen.getByTestId("reader-planner")).toBeVisible();

    fireEvent.click(screen.getByRole("button", { name: "Open Builder agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    // The discriminating assertion: dismissed on OPEN, not left mounted for
    // `Escape` to find.
    expect(screen.queryByTestId("reader-planner")).not.toBeInTheDocument();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    // And it does not come back, which would only move the collision one key
    // press later.
    expect(screen.queryByTestId("reader-planner")).not.toBeInTheDocument();
  });

  /**
   * Scenario: compare Planner's tab strip at tile size with the same strip
   * inside the pane. All five stay, because the tab set is a property of the
   * agent and `DeckSurface` owns the chosen tab keyed by agent id — a pane that
   * dropped tabs would have to coerce that shared state behind the user's back.
   */
  it("keeps all five panel tabs and the whole header at both presentations", () => {
    render(<DeckShell runtime={runtime()} />);
    const tile = screen.getByTestId("agent-tile-planner");
    expect(within(tile).getAllByRole("tab")).toHaveLength(5);

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    const overlay = screen.getByTestId("agent-pane-overlay");
    expect(within(overlay).getAllByRole("tab")).toHaveLength(5);
    expect(within(overlay).getByRole("heading", { name: "Planner" })).toBeVisible();
    // Nothing is removed from the header, and identity matters MORE here: in
    // the grid, position tells a reader which tile they are looking at.
    expect(within(overlay).getByText("ATT")).toBeVisible();
    expect(within(overlay).getByText("passed")).toBeVisible();
  });

  /**
   * Scenario: open Planner from the deck and read back the promoted tile's
   * presentation attribute. It is the seam every overlay CSS rule keys off, so
   * a tile promoted without it would be a full-window box still wearing the
   * grid's fixed terminal band.
   */
  it("marks the promoted tile with the overlay presentation and selects it", () => {
    render(<DeckShell runtime={runtime()} />);
    expect(screen.getByTestId("agent-tile-planner")).toHaveAttribute("data-presentation", "tile");

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.getByTestId("agent-tile-planner")).toHaveAttribute("data-presentation", "overlay");
    expect(screen.getByTestId("agent-tile-builder")).toHaveAttribute("data-presentation", "tile");
    // Opening SELECTS as well, which makes the pane's `selected` true rather
    // than merely degenerate — and settles `.agent-tile:not(.is-selected)
    // { display: none }` under 680px without relying on a specificity race.
    expect(screen.getByTestId("agent-tile-planner")).toHaveClass("is-selected");
  });

  /**
   * Scenario: open Planner and look for both pane controls at once. The pane
   * offers Back to dashboard, while the tiles behind it offer Open; only one
   * of these actions is live for Planner at a time.
   */
  it("offers Open on a tile and Back to dashboard on the pane, never both", () => {
    render(<DeckShell runtime={runtime()} />);
    expect(screen.queryByRole("button", { name: "Back to dashboard" })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.queryByRole("button", { name: "Open Planner agent" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Back to dashboard" })).toBeVisible();
    // The tiles underneath keep theirs — the deck is mounted, not frozen.
    expect(screen.getByRole("button", { name: "Open Builder agent" })).toBeInTheDocument();
  });

  /**
   * Scenario: open and close Planner over the deck after its four-terminal
   * shown set has been declared. Neither transition declares the unchanged set
   * again, so promoting a tile cannot create per-tile or per-pane ownership.
   */
  it("does not redeclare the unchanged deck shown set while the pane opens or closes", () => {
    const setShownTerminals = vi.fn(async () => undefined);
    render(<DeckShell runtime={runtime({ setShownTerminals })} />);

    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenLastCalledWith([{ deckId: FIXTURE_DAEMON_ID, agentId: "planner" }, { deckId: FIXTURE_DAEMON_ID, agentId: "builder" }, { deckId: FIXTURE_DAEMON_ID, agentId: "reviewer" }, { deckId: FIXTURE_DAEMON_ID, agentId: "tester" }]);
    setShownTerminals.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(setShownTerminals).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(setShownTerminals).not.toHaveBeenCalled();
  });

  /**
   * Scenario: start on the terminal-free overview, open Planner, then close it
   * again. Each changed render commit makes one whole-set declaration: empty,
   * Planner alone, then empty — never competing screen and pane declarations.
   */
  it("declares one whole shown set per overview pane transition", () => {
    const setShownTerminals = vi.fn(async () => undefined);
    render(<DeckShell runtime={runtime({ setShownTerminals })} initialView={{ kind: "overview" }} />);

    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenLastCalledWith([]);
    setShownTerminals.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Open Plan / architecture agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenLastCalledWith([{ deckId: FIXTURE_DAEMON_ID, agentId: "planner" }]);
    setShownTerminals.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Back to dashboard" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenLastCalledWith([]);
  });
});

/**
 * Issue #1400. With the `experimental` flag off — the shipped default — the
 * agent screen offers only what a live daemon supplies: the Terminal tab, and
 * TIME and TOOLS among the readings. The Diff, Checks, Delegations and
 * Artifacts tabs and the ATT, MODEL and USAGE fields come back with the flag.
 */
describe("agent details behind the experimental flag", () => {
  const DETAILS_OFF = { ...fixtureDesktopFeatures("?experimental=1"), showAgentDetails: false };

  beforeEach(() => {
    terminalBuilt.mockClear();
    window.localStorage.clear();
  });

  function liveRuntime(spawnedAtMs: number) {
    const snapshot = mapDesktopSnapshot({
      connection: { status: "connected", deckId: "deck-000000000000dec1", socketPath: "/tmp/deck.sock", deckKind: "local", clientProtocolVersion: 8, serverProtocolVersion: 8, clientBuildVersion: "0.1.0", daemonBuildVersion: "0.1.0" },
      agents: [{ id: "7", displayName: "Coder", cwd: "/tmp/project", rows: 32, cols: 120, agentType: "claude_code", status: "working", toolCount: 3, spawnedAtMs, tab: { kind: "dashboard" } }],
      protocolVersion: 8,
      source: "daemon",
    });
    return runtime({ mode: "live", desktopFeatures: undefined, snapshot, fleet: [snapshot] });
  }

  /**
   * Scenario: with the flag off, open a live agent's pane from the dashboard.
   * The tab strip holds Terminal alone, the header has no ATT, the readings
   * are TIME and TOOLS with no MODEL or USAGE, and TIME counts the agent's
   * uptime from the spawn instant the daemon reported.
   */
  it("shows only the terminal tab and the readings the daemon supplies while the flag is off", () => {
    const view = { kind: "agent" as const, deckId: "deck-000000000000dec1", agentId: "7", from: "overview" as const };
    render(<DeckShell runtime={liveRuntime(Date.now() - 5 * 60_000 - 5_000)} initialView={view} />);
    const pane = screen.getByTestId("agent-pane-overlay");

    expect(within(pane).getAllByRole("tab").map((tab) => tab.getAttribute("aria-label"))).toEqual(["Terminal"]);
    expect(within(pane).queryByText("ATT")).not.toBeInTheDocument();
    const metrics = within(pane).getByLabelText(/run metrics$/);
    expect(Array.from(metrics.children, (cell) => cell.querySelector("span")?.textContent)).toEqual(["TIME", "TOOLS"]);
    const time = within(metrics).getByText("TIME").parentElement!;
    expect(time).toHaveTextContent("5m");
    expect(time.getAttribute("title")).toMatch(/^Spawned by the daemon at: \d{4}-/);
  });

  /**
   * Scenario: the same live agent with the flag on. All five tabs and the
   * ATT, MODEL and USAGE fields are back, and TIME still reads the uptime.
   */
  it("brings back the four tabs and the ATT, MODEL and USAGE fields with the flag on", () => {
    const view = { kind: "agent" as const, deckId: "deck-000000000000dec1", agentId: "7", from: "overview" as const };
    render(<DeckShell runtime={{ ...liveRuntime(Date.now() - 2 * 3_600_000), desktopFeatures: fixtureDesktopFeatures("?experimental=1") }} initialView={view} />);
    const pane = screen.getByTestId("agent-pane-overlay");

    expect(within(pane).getAllByRole("tab").map((tab) => tab.getAttribute("aria-label"))).toEqual(["Terminal", "Diff", "Checks", "Delegations", "Artifacts"]);
    expect(within(pane).getByText("ATT")).toBeVisible();
    const metrics = within(pane).getByLabelText(/run metrics$/);
    expect(Array.from(metrics.children, (cell) => cell.querySelector("span")?.textContent)).toEqual(["TIME", "TOOLS", "MODEL", "USAGE"]);
    expect(within(metrics).getByText("TIME").parentElement).toHaveTextContent("2h");
  });

  /**
   * Scenario: open a deck of three live agents and let four minutes pass. All
   * three TIME readings count on ONE shared interval rather than one per tile,
   * and leaving the deck stops it.
   */
  it("counts every tile's TIME on one shared clock that stops with the deck", () => {
    vi.useFakeTimers();
    const started = vi.spyOn(window, "setInterval");
    const stopped = vi.spyOn(window, "clearInterval");
    try {
      vi.setSystemTime(new Date("2026-09-29T09:00:00.000Z").getTime());
      const spawnedAtMs = Date.now() - 60_000;
      const snapshot = mapDesktopSnapshot({
        connection: { status: "connected", deckId: "deck-000000000000dec1", socketPath: "/tmp/deck.sock", deckKind: "local", clientProtocolVersion: 8, serverProtocolVersion: 8, clientBuildVersion: "0.1.0", daemonBuildVersion: "0.1.0" },
        agents: ["7", "8", "9"].map((id) => ({ id, displayName: `Coder ${id}`, cwd: "/tmp/project", rows: 32, cols: 120, agentType: "claude_code", status: "working", toolCount: 0, spawnedAtMs, tab: { kind: "dashboard" as const } })),
        protocolVersion: 8,
        source: "daemon",
      });
      const { container, unmount } = render(<DeckShell runtime={runtime({ mode: "live", snapshot, fleet: [snapshot] })} />);
      const times = () => Array.from(container.querySelectorAll(".agent-instruments > div:first-child strong"), (cell) => cell.textContent);
      const tileTicks = () => started.mock.calls.filter(([, ms]) => ms === OVERVIEW_CLOCK_TICK_MS);

      expect(times()).toEqual(["1m", "1m", "1m"]);
      expect(tileTicks()).toHaveLength(1);

      act(() => { vi.advanceTimersByTime(4 * 60_000); });
      expect(times()).toEqual(["5m", "5m", "5m"]);
      expect(tileTicks()).toHaveLength(1);

      const interval = started.mock.results[started.mock.calls.indexOf(tileTicks()[0])]?.value;
      unmount();
      expect(stopped).toHaveBeenCalledWith(interval);
    } finally {
      vi.useRealTimers();
      vi.restoreAllMocks();
    }
  });

  /**
   * Scenario: pick Planner's Diff tab on the deck, then render the same deck
   * with the agent details hidden. Planner shows its terminal with the
   * Terminal tab selected rather than a Diff panel no tab selects, and the
   * deck declares Planner's terminal as shown again.
   */
  it("falls back to the terminal for an agent whose stored tab is hidden", () => {
    const setShownTerminals = vi.fn(async (_targets: AgentTarget[]) => undefined);
    const { rerender } = render(<DeckShell runtime={runtime({ setShownTerminals })} />);
    const tile = () => screen.getByTestId("agent-tile-planner");
    fireEvent.click(within(tile()).getByRole("tab", { name: "Diff" }));
    expect(within(tile()).queryByTestId("terminal-planner")).not.toBeInTheDocument();
    expect(setShownTerminals.mock.lastCall?.[0]).not.toContainEqual({ deckId: FIXTURE_DAEMON_ID, agentId: "planner" });

    rerender(<DeckShell runtime={runtime({ setShownTerminals, desktopFeatures: DETAILS_OFF })} />);

    expect(within(tile()).getAllByRole("tab").map((tab) => tab.getAttribute("aria-label"))).toEqual(["Terminal"]);
    expect(within(tile()).getByRole("tab", { name: "Terminal" })).toHaveAttribute("aria-selected", "true");
    expect(within(tile()).getByTestId("terminal-planner")).toBeInTheDocument();
    expect(tile().querySelector(".diff-panel")).toBeNull();
    expect(setShownTerminals.mock.lastCall?.[0]).toContainEqual({ deckId: FIXTURE_DAEMON_ID, agentId: "planner" });
  });
});

/**
 * Issue #1676. The daemon tells an agent that finished its turn (`idle`) from
 * one that is waiting on the user (`waiting_for_input`), and the TUI shows them
 * as Idle and Needs Input. The desktop must too, on every screen that shows an
 * agent's status, or an idle agent reads as one that needs you.
 */
describe("idle and needs input are two states (issue #1676)", () => {
  beforeEach(() => {
    terminalBuilt.mockClear();
    window.localStorage.clear();
  });

  function liveRuntime() {
    const snapshot = mapDesktopSnapshot({
      connection: { status: "connected", deckId: "deck-000000000000dec1", socketPath: "/tmp/deck.sock", deckKind: "local", clientProtocolVersion: 8, serverProtocolVersion: 8, clientBuildVersion: "0.1.0", daemonBuildVersion: "0.1.0" },
      agents: ([["1", "Resting", "idle"], ["2", "Asking", "waiting_for_input"], ["3", "Unheard", "unknown"]] as const).map(([id, displayName, status]) => ({ id, displayName, cwd: "/tmp/project", rows: 32, cols: 120, agentType: "claude_code", status, toolCount: 0, tab: { kind: "dashboard" as const } })),
      protocolVersion: 8,
      source: "daemon",
    });
    return runtime({ mode: "live", snapshot, fleet: [snapshot] });
  }

  /** The status word each agent's own status label reads, by agent name. */
  function labels(cards: HTMLElement[], nameOf: (card: HTMLElement) => string | null | undefined): Record<string, string> {
    return Object.fromEntries(cards.map((card) => [nameOf(card) ?? "", card.querySelector(".status-label")?.textContent ?? ""]));
  }

  /**
   * Scenario: open the agent dashboard on a daemon with an idle agent, an agent
   * waiting for the user and an agent in a status the daemon calls unknown. The
   * rows read idle, needs input and idle, as the TUI's cards read Idle, Needs
   * Input and Idle, and the header counts one needing input and two idle.
   */
  it("labels an idle agent and one that needs the user differently on the dashboard", () => {
    render(<AppDeckShell runtime={liveRuntime()} initialView={{ kind: "overview" }} />);
    const rows = screen.getAllByRole("row").filter((row) => row.classList.contains("overview-row"));

    expect(labels(rows, (row) => row.querySelector(".overview-agent-name strong")?.textContent)).toEqual({ Resting: "idle", Asking: "needs input", Unheard: "idle" });
    expect(screen.getByTestId("overview-count-needs-input")).toHaveTextContent("1");
    expect(screen.getByTestId("overview-count-idle")).toHaveTextContent("2");
  });

  /**
   * Scenario: open the deck with the same three agents, then open the agent
   * that needs the user. Each tile's status reads idle or needs input, and the
   * agent screen's header keeps saying needs input.
   */
  it("labels them differently on the deck's tiles and on the agent screen", () => {
    const deck = liveRuntime();
    const { unmount } = render(<DeckShell runtime={deck} />);
    // Every tile is titled by its agent type, so they are read in the daemon's order.
    const tiles = Array.from(document.querySelectorAll<HTMLElement>("article.agent-tile"));

    expect(tiles.map((tile) => tile.querySelector(".status-label")?.textContent)).toEqual(["idle", "needs input", "idle"]);
    unmount();

    render(<DeckShell runtime={deck} initialView={{ kind: "agent", deckId: "deck-000000000000dec1", agentId: "2", from: "overview" }} />);
    expect(within(screen.getByTestId("agent-pane-overlay")).getAllByText("needs input")[0]).toHaveClass("status-label");
  });
});
