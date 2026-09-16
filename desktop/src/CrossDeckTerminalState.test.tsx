import { act, render, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { agentKey } from "./lib/agentKey";
import type { DeckBridge } from "./lib/bridge";
import type { DeckSnapshot, TerminalChunk } from "./types";

/*
 * A recording xterm. The real one needs WebGL and a box model, neither of which
 * jsdom has — and what is under test here is precisely which BYTES reach the
 * terminal, so `write` has to be observable rather than merely survivable.
 *
 * Hoisted so the `vi.mock` factories below — which vitest lifts above the
 * imports — can close over it.
 */
const { writes, FakeTerminal, FakeFitAddon } = vi.hoisted(() => {
  const writes: string[] = [];
  const decoder = new TextDecoder();
  class FakeTerminal {
    options: Record<string, unknown>;
    textarea: HTMLTextAreaElement;
    cols = 80;
    rows = 24;
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      this.textarea = document.createElement("textarea");
    }
    loadAddon(addon: { activate?: (terminal: FakeTerminal) => void }): void { addon.activate?.(this); }
    open(host: HTMLElement): void { host.appendChild(this.textarea); }
    write(data: string | Uint8Array): void {
      writes.push(typeof data === "string" ? data : decoder.decode(data));
    }
    reset(): void {}
    focus(): void {}
    resize(cols: number, rows: number): void { this.cols = cols; this.rows = rows; }
    onData(): { dispose: () => void } { return { dispose: () => {} }; }
    dispose(): void {}
  }
  class FakeFitAddon {
    activate(): void {}
    fit(): void {}
    dispose(): void {}
  }
  return { writes, FakeTerminal, FakeFitAddon };
});

vi.mock("@xterm/xterm", () => ({ Terminal: FakeTerminal as unknown as typeof import("@xterm/xterm").Terminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: FakeFitAddon as unknown as typeof import("@xterm/addon-fit").FitAddon }));
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class {
    onContextLoss(): void {}
    dispose(): void {}
  } as unknown as typeof import("@xterm/addon-webgl").WebglAddon,
}));

/* A bridge stub, so the test can push a terminal chunk and a fleet by hand. */
const { bridge } = vi.hoisted(() => ({
  bridge: {
    mode: "fixture",
    connect: vi.fn(),
    subscribe: vi.fn(async (_onFleet: (fleet: unknown) => void, _onTerminal: (event: TerminalChunk) => void) => () => {}),
    runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => {}),
    resizeTerminal: vi.fn(async () => {}),
    onTerminalGeometry: vi.fn((_listener: (agentId: string, rows: number, cols: number, deckId?: string) => void) => () => {}),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: {} })),
    saveSettings: vi.fn(async (settings: unknown) => settings),
    testEndpoint: vi.fn(),
    setShownTerminals: vi.fn(async () => {}),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(),
    dispose: vi.fn(async () => {}),
  },
}));

vi.mock("./lib/bridge", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/bridge")>()),
  selectRuntimeMode: () => "fixture" as const,
  createDeckBridge: () => bridge as unknown as DeckBridge,
}));

import { TerminalViewport } from "./components/TerminalViewport";
import { useDeckRuntime } from "./hooks/useDeckRuntime";

/** The second deck, running the same per-daemon monotonic agent ids as the first. */
const REMOTE_DECK_ID = "deck-00000000000000b2";
/** Bytes only deck A's `planner` ever produced. Nothing on deck B may show them. */
const DECK_A_OUTPUT = "sentinel-output-that-belongs-to-deck-a";

function remoteDeck(): DeckSnapshot {
  const local = createFixtureSnapshot("connected");
  return {
    ...local,
    connection: { ...local.connection, deckId: REMOTE_DECK_ID, socketPath: "dev@build-box", deckKind: "remote" },
    agents: local.agents.map((agent) => ({ ...agent, daemonId: REMOTE_DECK_ID })),
  };
}

/**
 * PRD #1105's security audit, BLOCKER 2 — the ordinary cross-deck path replayed
 * the previous deck's terminal buffer under the requested agent.
 *
 * The mechanism, reproduced here rather than described: the runtime's retained
 * PTY buffers were keyed by **bare** agent id; leaving a deck detaches its
 * sessions and clears no buffer; and `TerminalViewport`'s `!previous` branch
 * writes whatever `terminalFeed.get` answers straight into a freshly mounted
 * xterm. So opening deck B's `planner` — an id both decks mint, because ids are
 * per-daemon monotonic — showed up to the feed's 1 MiB retention of deck A's
 * output under B's correctly resolved heading, with B's live transcript empty
 * and nothing on screen saying which machine it came from. Where the switch is
 * refused or the attach fails, no replacement stream ever arrives and it stays.
 *
 * Delaying the shown-set declaration does not touch this: the viewport's
 * backlog read happens at mount, before anything is attached at all.
 */
describe("cross-deck terminal state", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    writes.length = 0;
    bridge.connect.mockResolvedValue([createFixtureSnapshot("connected")]);
    bridge.onTerminalGeometry.mockReturnValue(() => {});
  });

  /**
   * Scenario: deck A's Planner streams output, the app is then retargeted to
   * deck B — which runs an agent of the same id — and a viewport mounts for
   * B's Planner. It reads its backlog from the feed at mount, and deck A's
   * bytes must not be in it.
   */
  it("does not replay the previous deck's buffer under the next deck's namesake", async () => {
    let feedTerminal: ((event: TerminalChunk) => void) | undefined;
    let pushFleet: ((fleet: unknown) => void) | undefined;
    bridge.subscribe.mockImplementation(async (onFleet, onTerminal) => {
      pushFleet = onFleet;
      feedTerminal = onTerminal;
      return () => {};
    });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    // Deck A's Planner produces output. The bridge stamps the deck it was on.
    act(() => feedTerminal?.({
      agentId: "planner",
      deckId: FIXTURE_DAEMON_ID,
      data: new TextEncoder().encode(DECK_A_OUTPUT),
      stream: "output",
      operation: "replace",
      generation: 1,
    }));

    // The app is retargeted: deck B is now the selected one, and it runs a
    // `planner` too. Nothing clears the retained buffers — that is the
    // premise, not an omission.
    act(() => pushFleet?.([remoteDeck()]));

    const paneForDeckB = render(
      <TerminalViewport
        agentId="planner"
        deckId={REMOTE_DECK_ID}
        label="Planner"
        transcript=""
        terminalFeed={result.current.terminalFeed}
        onInput={() => {}}
        onResize={() => {}}
      />,
    );

    expect(writes.join("")).not.toContain(DECK_A_OUTPUT);
    paneForDeckB.unmount();

    // The positive control, so this is not passing by delivering nothing at
    // all: the same buffer still reaches the deck it actually belongs to.
    writes.length = 0;
    render(
      <TerminalViewport
        agentId="planner"
        deckId={FIXTURE_DAEMON_ID}
        label="Planner"
        transcript=""
        terminalFeed={result.current.terminalFeed}
        onInput={() => {}}
        onResize={() => {}}
      />,
    );
    expect(writes.join("")).toContain(DECK_A_OUTPUT);
  });

  /**
   * Scenario: read the same buffer back through the feed directly, under each
   * deck. This states the property the test above observes through a terminal —
   * that the feed is addressed by the composite `(deckId, agentId)` — as a fact
   * about the runtime, so a future viewport is not the only thing holding it.
   */
  it("keys retained buffers by the composite identity, not by the bare agent id", async () => {
    let feedTerminal: ((event: TerminalChunk) => void) | undefined;
    bridge.subscribe.mockImplementation(async (_onFleet, onTerminal) => {
      feedTerminal = onTerminal;
      return () => {};
    });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    act(() => feedTerminal?.({
      agentId: "planner",
      deckId: FIXTURE_DAEMON_ID,
      data: new TextEncoder().encode(DECK_A_OUTPUT),
      stream: "output",
      operation: "replace",
      generation: 1,
    }));

    expect(result.current.terminalFeed?.get(FIXTURE_DAEMON_ID, "planner")).toBeDefined();
    expect(result.current.terminalFeed?.get(REMOTE_DECK_ID, "planner")).toBeUndefined();
    expect(agentKey(FIXTURE_DAEMON_ID, "planner")).not.toBe(agentKey(REMOTE_DECK_ID, "planner"));
  });

  /**
   * Scenario: a non-delivered verdict is recorded for deck A's Planner, and the
   * app is then retargeted to deck B. Reading the verdict under B's Planner
   * finds nothing — the record describes an attempt against another machine's
   * agent, and letting it through would disable B's pane and print A's
   * rejection notice under B's heading, with no bounded clear on a refused
   * switch. (PRD #1105's security audit, should-fix: same root cause.)
   */
  it("keys recorded send verdicts by the composite identity too", async () => {
    bridge.subscribe.mockImplementation(async () => () => {});
    bridge.runAction.mockResolvedValue({ ok: false, sendResult: "wrong-session" });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "hello" });
    });

    expect(result.current.terminalInputResults?.[agentKey(FIXTURE_DAEMON_ID, "planner")]).toBe("wrong-session");
    expect(result.current.terminalInputResults?.[agentKey(REMOTE_DECK_ID, "planner")]).toBeUndefined();
  });

  /**
   * Scenario: the daemon reports an applied geometry for deck A's Planner, then
   * the app is retargeted to deck B. B's Planner has no geometry of its own
   * yet, and must not inherit A's — a pane that applied it would submit another
   * machine's grid to this agent's PTY, reflowing it and every other viewer
   * attached to it. (PRD #1105's security audit, should-fix: same root cause.)
   */
  it("keys applied geometry by the composite identity too", async () => {
    let pushGeometry: ((agentId: string, rows: number, cols: number, deckId?: string) => void) | undefined;
    bridge.subscribe.mockImplementation(async () => () => {});
    bridge.onTerminalGeometry.mockImplementation((listener) => {
      pushGeometry = listener;
      return () => {};
    });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    act(() => pushGeometry?.("planner", 24, 80, FIXTURE_DAEMON_ID));

    expect(result.current.appliedGeometry?.[agentKey(FIXTURE_DAEMON_ID, "planner")]).toEqual({ rows: 24, cols: 80 });
    expect(result.current.appliedGeometry?.[agentKey(REMOTE_DECK_ID, "planner")]).toBeUndefined();
  });
});
