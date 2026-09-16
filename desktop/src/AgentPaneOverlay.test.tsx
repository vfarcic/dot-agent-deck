import { fireEvent, render, screen, within } from "@testing-library/react";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentTileProps } from "./components/AgentTile";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckActionResult, DeckRuntimeState } from "./types";

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

import { DeckShell } from "./App";

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

    fireEvent.click(screen.getByRole("button", { name: "Close Planner agent" }));
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
   * Scenario: open Planner and look for both pane controls at once. Only the
   * open pane offers Close, and only the tiles behind it offer Open, so the two
   * can never both be live for one agent.
   */
  it("offers Open on a tile and Close on the pane, never both", () => {
    render(<DeckShell runtime={runtime()} />);
    expect(screen.queryByRole("button", { name: "Close Planner agent" })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.queryByRole("button", { name: "Open Planner agent" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Close Planner agent" })).toBeVisible();
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
    expect(setShownTerminals).toHaveBeenLastCalledWith(["planner", "builder", "reviewer", "tester"]);
    setShownTerminals.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Open Planner agent" }));
    expect(screen.getByTestId("agent-pane-overlay")).toBeVisible();
    expect(setShownTerminals).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Close Planner agent" }));
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
    expect(setShownTerminals).toHaveBeenLastCalledWith(["planner"]);
    setShownTerminals.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "Close Planner agent" }));
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(setShownTerminals).toHaveBeenCalledTimes(1);
    expect(setShownTerminals).toHaveBeenLastCalledWith([]);
  });
});
