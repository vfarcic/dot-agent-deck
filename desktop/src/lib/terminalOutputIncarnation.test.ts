import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Terminal } from "@xterm/xterm";
import type { DesktopSnapshotDto } from "./bridge";
import { registerTerminal, unregisterTerminal } from "./terminalRegistry";
import type { AgentTarget, TerminalChunk } from "../types";

/*
 * PRD #1666 part 2 (G-fix7) — what leaves the bridge for the runtime and the
 * xterm carries the daemon process as well as the deck: every chunk is stamped
 * with its session's incarnation, the applied grid is pushed with it, and an
 * attach's grid is applied only to the xterm registered for that process —
 * and only once the attach is known to still be the current one.
 */

const invoke = vi.fn();
const listeners = new Map<string, (event: { payload: unknown }) => void>();

class MockChannel<T> {
  onmessage?: (value: T) => void;
}

vi.mock("@tauri-apps/api/core", () => ({ invoke, Channel: MockChannel }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    listeners.set(name, callback);
    return () => listeners.delete(name);
  }),
}));

const DECK = "deck-000000000000dec1";

const snapshot: DesktopSnapshotDto = {
  connection: {
    status: "connected",
    deckId: DECK,
    socketPath: "/tmp/deck.sock",
    deckKind: "local",
    clientProtocolVersion: 6,
    serverProtocolVersion: 6,
    clientBuildVersion: "0.1.0",
    daemonBuildVersion: "0.1.0",
    runningAgentCount: 1,
  },
  agents: [{ id: "agent-1", paneId: "pane-1", displayName: "Builder", cwd: "/tmp/project", rows: 32, cols: 120, agentType: "codex", status: "working", toolCount: 0, tab: { kind: "dashboard" }, incarnation: "process-b" }],
  protocolVersion: 6,
  source: "daemon",
};

const as = (incarnation: string): AgentTarget => ({ deckId: DECK, agentId: "agent-1", incarnation });
const A = as("process-a");
const B = as("process-b");

type Attach = { sessionId: string; agentId: string; generation: number; reused: boolean; appliedRows?: number; appliedCols?: number };

/** Each process's attach answers as `attach` says; its output channel is kept so a test can write through it. */
function backend(attach: Record<string, () => Promise<Attach>>) {
  const channels = new Map<string, MockChannel<number[]>>();
  invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === "desktop_bootstrap") return snapshot;
    if (command === "desktop_terminal_attach") {
      const incarnation = args?.incarnation as string;
      channels.set(incarnation, args?.onOutput as MockChannel<number[]>);
      return attach[incarnation]();
    }
    return { ok: true };
  });
  return channels;
}

/** A stand-in xterm: only what the bridge's grid application reads and calls. */
function fakeTerminal(): Terminal & { resize: ReturnType<typeof vi.fn> } {
  return { cols: 80, rows: 24, resize: vi.fn() } as unknown as Terminal & { resize: ReturnType<typeof vi.fn> };
}

describe("TauriDeckBridge output and grid carry the incarnation (G-fix7)", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.clear();
  });

  afterEach(() => vi.restoreAllMocks());

  /**
   * Scenario: the successor's attach replays its scrollback and then streams.
   * Every chunk handed to the runtime names the successor's process as well
   * as its deck, so the runtime files it under the successor and never under
   * the agent the id used to name.
   */
  it("stamps every chunk with the process of the session that produced it", async () => {
    const channels = backend({ "process-b": async () => ({ sessionId: "session-b", agentId: "agent-1", generation: 2, reused: false }) });
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    const output = vi.fn<(chunk: TerminalChunk) => void>();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    await bridge.setShownTerminals([B]);
    channels.get("process-b")?.onmessage?.(Array.from(new TextEncoder().encode("successor bytes")));

    await vi.waitFor(() => expect(output.mock.calls.length).toBeGreaterThanOrEqual(2));
    for (const [chunk] of output.mock.calls) {
      expect(chunk).toMatchObject({ agentId: "agent-1", deckId: DECK, incarnation: "process-b" });
    }
    await bridge.dispose();
  });

  /**
   * Scenario: an attach for the old process is still pending when the
   * successor is declared under the same id; the old attach then settles with
   * an applied grid. Nothing is reshaped and no geometry is announced for it —
   * the attempt was evicted, so it is detached before it can touch a terminal.
   * The successor's own attach (control) reshapes the successor's xterm and is
   * announced with the successor's process.
   */
  it("applies an evicted attach's grid to nothing", async () => {
    let settleA!: (value: Attach) => void;
    backend({
      "process-a": () => new Promise((resolve) => { settleA = resolve; }),
      "process-b": async () => ({ sessionId: "session-b", agentId: "agent-1", generation: 2, reused: false, appliedRows: 30, appliedCols: 100 }),
    });
    const successor = fakeTerminal();
    registerTerminal(DECK, "agent-1", successor, "process-b");
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    const geometry = vi.fn();
    await bridge.subscribe(vi.fn(), vi.fn());
    bridge.onTerminalGeometry(geometry);
    await bridge.connect();
    try {
      const first = bridge.setShownTerminals([A]);
      await vi.waitFor(() => expect(invoke.mock.calls.filter(([name]) => name === "desktop_terminal_attach")).toHaveLength(1));
      await bridge.setShownTerminals([B]);
      expect(successor.resize).toHaveBeenCalledWith(100, 30);
      expect(geometry).toHaveBeenCalledWith("agent-1", 30, 100, DECK, "process-b");
      successor.resize.mockClear();
      geometry.mockClear();

      settleA({ sessionId: "session-a", agentId: "agent-1", generation: 1, reused: false, appliedRows: 48, appliedCols: 160 });
      await first;
      await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("desktop_terminal_detach", { sessionId: "session-a" }));
      expect(successor.resize, "an evicted attach must not reshape the current xterm").not.toHaveBeenCalled();
      expect(geometry, "an evicted attach must not announce a grid").not.toHaveBeenCalled();
    } finally {
      unregisterTerminal(DECK, "agent-1", successor, "process-b");
      await bridge.dispose();
    }
  });

  /**
   * Scenario: the old process's pane and the successor's are both declared
   * (a held pane over the deck grid), and the old attach answers with its own
   * grid. Only an xterm registered for the old process could take it; the
   * successor's xterm, registered under the same deck and id, is not touched.
   */
  it("reshapes only the xterm of the process the attach was for", async () => {
    backend({
      "process-a": async () => ({ sessionId: "session-a", agentId: "agent-1", generation: 1, reused: false, appliedRows: 48, appliedCols: 160 }),
      "process-b": async () => ({ sessionId: "session-b", agentId: "agent-1", generation: 2, reused: false }),
    });
    const successor = fakeTerminal();
    registerTerminal(DECK, "agent-1", successor, "process-b");
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    const geometry = vi.fn();
    await bridge.subscribe(vi.fn(), vi.fn());
    bridge.onTerminalGeometry(geometry);
    await bridge.connect();
    try {
      await bridge.setShownTerminals([A, B]);
      await vi.waitFor(() => expect(geometry).toHaveBeenCalledWith("agent-1", 48, 160, DECK, "process-a"));
      expect(successor.resize, "the old process's grid must not reach the successor's xterm").not.toHaveBeenCalled();
    } finally {
      unregisterTerminal(DECK, "agent-1", successor, "process-b");
      await bridge.dispose();
    }
  });
});
