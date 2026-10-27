import { act, render, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { boundAgentKey } from "./lib/agentKey";
import type { DeckBridge } from "./lib/bridge";
import type { TerminalChunk } from "./types";

/*
 * A recording xterm. The real one needs WebGL and a box model, neither of which
 * jsdom has — and what is under test here is precisely which BYTES reach the
 * terminal, so `write` has to be observable rather than merely survivable.
 *
 * Hoisted so the `vi.mock` factories below — which vitest lifts above the
 * imports — can close over it.
 */
const { writes, terminalInstances, FakeTerminal, FakeFitAddon } = vi.hoisted(() => {
  const writes: string[] = [];
  const terminalInstances: FakeTerminal[] = [];
  const decoder = new TextDecoder();
  const inertHandler = (): { dispose: () => void } => ({ dispose: () => {} });
  class FakeTerminal {
    options: Record<string, unknown>;
    textarea: HTMLTextAreaElement;
    writes: string[] = [];
    cols = 80;
    rows = 24;
    constructor(options: Record<string, unknown>) {
      this.options = { ...options };
      this.textarea = document.createElement("textarea");
      terminalInstances.push(this);
    }
    loadAddon(addon: { activate?: (terminal: FakeTerminal) => void }): void { addon.activate?.(this); }
    open(host: HTMLElement): void { host.appendChild(this.textarea); }
    write(data: string | Uint8Array, callback?: () => void): void {
      const decoded = typeof data === "string" ? data : decoder.decode(data);
      // Issue #1705's replay brackets are empty writes; they carry no output.
      if (decoded) {
        writes.push(decoded);
        this.writes.push(decoded);
      }
      callback?.();
    }
    /* Issue #1705's OSC 52 handler; `TerminalViewport.clipboard.test.tsx` drives it through the real xterm. */
    readonly parser = { registerOscHandler: inertHandler, registerCsiHandler: inertHandler, registerDcsHandler: inertHandler };
    reset(): void {}
    focus(): void {}
    resize(cols: number, rows: number): void { this.cols = cols; this.rows = rows; }
    onData(): { dispose: () => void } { return { dispose: () => {} }; }
    /* The scrollback listeners `TerminalViewport` registers (issue #1492); nothing here scrolls. */
    readonly buffer = { active: { baseY: 0 }, onBufferChange: (): { dispose: () => void } => ({ dispose: () => {} }) };
    onScroll(): { dispose: () => void } { return { dispose: () => {} }; }
    onWriteParsed(): { dispose: () => void } { return { dispose: () => {} }; }
    attachCustomKeyEventHandler(): void {}
    // Issue #1457 — the selection keeper's subscriptions.
    onResize(): { dispose: () => void } { return { dispose: () => {} }; }
    onSelectionChange(): { dispose: () => void } { return { dispose: () => {} }; }
    dispose(): void {}
  }
  class FakeFitAddon {
    activate(): void {}
    fit(): void {}
    dispose(): void {}
  }
  return { writes, terminalInstances, FakeTerminal, FakeFitAddon };
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
    onTerminalGeometry: vi.fn((_listener: (agentId: string, rows: number, cols: number, deckId?: string, incarnation?: string) => void) => () => {}),
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
import { getTerminal } from "./lib/terminalRegistry";

/** Bytes only the old process's `planner` produced. Its successor under the same id must never show them. */
const OLD_OUTPUT = "sentinel-output-of-the-process-before-the-restart";
const GONE = "that agent is gone (the deck restarted)";

/**
 * PRD #1666 part 2 (G-fix7) — the runtime's replay buffers, its applied grid
 * and every xterm are keyed by the agent AS ONE DAEMON PROCESS LISTED IT.
 *
 * A restarted deck — the same build included — numbers its agents from 1
 * again, so after a restart `(deckId, agentId)` names an unrelated agent too.
 * Keyed by that pair alone, a freshly opened successor read the previous
 * process's retained backlog at mount and showed it under its own heading, for
 * as long as its own attach was pending — and indefinitely when that attach
 * failed, since the failure was appended to the same buffer.
 */
describe("terminal state keyed by the daemon process", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    writes.length = 0;
    terminalInstances.length = 0;
    bridge.connect.mockResolvedValue([createFixtureSnapshot("connected")]);
    bridge.onTerminalGeometry.mockReturnValue(() => {});
  });

  /**
   * Scenario: the old process's Planner streams output; the deck restarts and
   * a pane is opened for the successor Planner, under the same deck and id.
   * Its attach fails while a late old-process chunk arrives. The pane shows
   * the failure and none of the old process's bytes; a pane for the old
   * process (control) still replays them.
   */
  it("never replays the previous process's output under its successor", async () => {
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
      incarnation: "process-a",
      data: new TextEncoder().encode(OLD_OUTPUT),
      stream: "output",
      operation: "replace",
      generation: 1,
    }));

    const successor = render(
      <TerminalViewport agentId="planner" deckId={FIXTURE_DAEMON_ID} incarnation="process-b" label="Planner" transcript="" terminalFeed={result.current.terminalFeed} onInput={() => {}} onResize={() => {}} />,
    );
    expect(writes.join("")).not.toContain(OLD_OUTPUT);

    act(() => feedTerminal?.({ agentId: "planner", deckId: FIXTURE_DAEMON_ID, incarnation: "process-a", data: new TextEncoder().encode(OLD_OUTPUT), stream: "output", operation: "append", generation: 1 }));
    expect(writes.join(""), "a late old-process chunk must not notify the successor's listener").not.toContain(OLD_OUTPUT);

    // The successor's attach fails: the sentence lands on the successor's own
    // buffer, and it carries no part of the old process's backlog.
    act(() => feedTerminal?.({ agentId: "planner", deckId: FIXTURE_DAEMON_ID, incarnation: "process-b", data: new Uint8Array(), stream: "error", operation: "append", message: GONE }));
    expect(writes.join("")).toContain(GONE);
    expect(writes.join("")).not.toContain(OLD_OUTPUT);
    const successorBuffer = new TextDecoder().decode(result.current.terminalFeed?.get(FIXTURE_DAEMON_ID, "planner", "process-b")?.data);
    expect(successorBuffer).not.toContain(OLD_OUTPUT);
    expect(new TextDecoder().decode(result.current.terminalFeed?.get(FIXTURE_DAEMON_ID, "planner", "process-a")?.data)).not.toContain(GONE);
    successor.unmount();

    // Control: the old process's own pane still replays its backlog.
    writes.length = 0;
    render(<TerminalViewport agentId="planner" deckId={FIXTURE_DAEMON_ID} incarnation="process-a" label="Planner" transcript="" terminalFeed={result.current.terminalFeed} onInput={() => {}} onResize={() => {}} />);
    expect(writes.join("")).toContain(OLD_OUTPUT);
  });

  /**
   * Scenario: a pane shows the old process's Planner, and the row it renders
   * is replaced by the successor's under the same deck and id. The pane gets a
   * new xterm, registered for the successor only, and the old one's bytes are
   * not in it. A re-render for the same process (control) keeps the xterm.
   */
  it("gives a successor under the same id an xterm of its own", async () => {
    let feedTerminal: ((event: TerminalChunk) => void) | undefined;
    bridge.subscribe.mockImplementation(async (_onFleet, onTerminal) => {
      feedTerminal = onTerminal;
      return () => {};
    });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));
    act(() => feedTerminal?.({ agentId: "planner", deckId: FIXTURE_DAEMON_ID, incarnation: "process-a", data: new TextEncoder().encode(OLD_OUTPUT), stream: "output", operation: "replace", generation: 1 }));

    const pane = (incarnation: string) => <TerminalViewport agentId="planner" deckId={FIXTURE_DAEMON_ID} incarnation={incarnation} label="Planner" transcript="" terminalFeed={result.current.terminalFeed} onInput={() => {}} onResize={() => {}} />;
    const view = render(pane("process-a"));
    view.rerender(pane("process-a"));
    expect(terminalInstances, "the same process keeps its xterm").toHaveLength(1);
    expect(terminalInstances[0].writes.join("")).toContain(OLD_OUTPUT);

    view.rerender(pane("process-b"));
    expect(terminalInstances).toHaveLength(2);
    expect(terminalInstances[1].writes.join("")).not.toContain(OLD_OUTPUT);
    expect(getTerminal(FIXTURE_DAEMON_ID, "planner", "process-b")).toBe(terminalInstances[1]);
    expect(getTerminal(FIXTURE_DAEMON_ID, "planner", "process-a")).toBeUndefined();
    view.unmount();
  });

  /**
   * Scenario: the daemon applies a grid to the old process's Planner. The
   * successor under the same deck and id has no grid of its own yet and does
   * not inherit that one; the old process's entry (control) holds it.
   */
  it("keeps an applied grid to the process it was applied for", async () => {
    let pushGeometry: ((agentId: string, rows: number, cols: number, deckId?: string, incarnation?: string) => void) | undefined;
    bridge.subscribe.mockImplementation(async () => () => {});
    bridge.onTerminalGeometry.mockImplementation((listener) => {
      pushGeometry = listener;
      return () => {};
    });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    act(() => pushGeometry?.("planner", 48, 160, FIXTURE_DAEMON_ID, "process-a"));

    expect(result.current.appliedGeometry?.[boundAgentKey({ deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: "process-a" })]).toEqual({ rows: 48, cols: 160 });
    expect(result.current.appliedGeometry?.[boundAgentKey({ deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: "process-b" })]).toBeUndefined();
  });
});
