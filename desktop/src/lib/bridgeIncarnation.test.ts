import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopSnapshotDto } from "./bridge";
import type { AgentTarget } from "../types";

/*
 * PRD #1666 part 2 (G-fix6) — the bridge's terminal caches are keyed by the
 * agent AND the incarnation it was listed with. A restarted deck reuses agent
 * ids, so a session attached for one daemon process must never be handed to a
 * target from another: not by the shown declaration, not by input, not by a
 * resize, and not by an attach that settles after the target moved on.
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
const GONE = "that agent is gone (the deck restarted)";

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

/** The backend's half: an attach bound to the old process is refused with the gone sentence; the successor's is accepted. */
function backend(attachA?: () => Promise<unknown>) {
  invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === "desktop_bootstrap") return snapshot;
    if (command === "desktop_terminal_attach") {
      if (args?.incarnation === "process-a") {
        if (attachA) return attachA();
        throw new Error(GONE);
      }
      return { sessionId: "session-b", agentId: "agent-1", generation: 2, reused: false };
    }
    return { ok: true };
  });
}

const calls = (command: string) => invoke.mock.calls.filter(([name]) => name === command);

describe("TauriDeckBridge terminal caches carry the incarnation (G-fix6)", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.clear();
    backend();
  });

  afterEach(() => vi.restoreAllMocks());

  /** Scenario: the successor's session is installed for agent-1, then a stale card of the old process declares the same agent. The bridge attaches for the old process (which the backend refuses) instead of handing it the successor's session, and its input rejects without writing; the successor's own input, the control, reaches its session. */
  it("never hands a stale target the successor's installed session", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    await bridge.setShownTerminals([B]);
    // Control: the same incarnation reuses the installed session — no second attach.
    await bridge.setShownTerminals([B]);
    expect(calls("desktop_terminal_attach")).toHaveLength(1);
    await bridge.sendTerminalInput(B, "successor-input");
    expect(calls("desktop_terminal_write")).toEqual([["desktop_terminal_write", { sessionId: "session-b", data: Array.from(new TextEncoder().encode("successor-input")) }]]);
    // A stale card's keystrokes, with the successor's session installed under the same pair, select no session.
    await expect(bridge.sendTerminalInput(A, "stale-before-declare")).rejects.toThrow("Terminal for agent-1 is not attached.");
    expect(calls("desktop_terminal_write"), "a stale card's input must not select the successor's session").toHaveLength(1);

    await bridge.setShownTerminals([A]);
    expect(calls("desktop_terminal_attach").map(([, args]) => (args as { incarnation: unknown }).incarnation), "the stale target must be attached for its own process, not given the successor's session").toEqual(["process-b", "process-a"]);
    await expect(bridge.sendTerminalInput(A, "stale-input")).rejects.toThrow("Terminal for agent-1 is not attached.");
    expect(calls("desktop_terminal_write"), "nothing typed into the stale card may reach the successor").toHaveLength(1);
    expect(JSON.stringify(output.mock.calls)).toContain(GONE);
    await bridge.dispose();
  });

  /** Scenario: an attach for the old process is still in flight when the next declaration names the successor under the same id. The late attach detaches instead of installing, the stale target stays unattached, and the successor's attach installs its own session. */
  it("does not install an attach whose target's incarnation was superseded while it was pending", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    let settle!: (value: unknown) => void;
    backend(() => new Promise((resolve) => { settle = resolve; }));
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    const first = bridge.setShownTerminals([A]);
    await vi.waitFor(() => expect(calls("desktop_terminal_attach")).toHaveLength(1));
    await bridge.setShownTerminals([B]);
    settle({ sessionId: "session-a", agentId: "agent-1", generation: 1, reused: false });
    await first;
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("desktop_terminal_detach", { sessionId: "session-a" }));
    await expect(bridge.sendTerminalInput(A, "late")).rejects.toThrow("Terminal for agent-1 is not attached.");
    await bridge.sendTerminalInput(B, "now");
    expect(calls("desktop_terminal_write").map(([, args]) => (args as { sessionId: string }).sessionId)).toEqual(["session-b"]);
    await bridge.dispose();
  });

  /** Scenario: with the successor's session installed, a stale card of the old process reports a terminal size. Nothing is resized; the successor's own resize, the control, reaches its session. */
  it("sends a stale target's resize to no session", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    await bridge.setShownTerminals([B]);
    await bridge.resizeTerminal(A, 90, 30);
    await new Promise((resolve) => window.requestAnimationFrame(() => resolve(undefined)));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(calls("desktop_terminal_resize"), "a stale card's size must not reflow the successor's PTY").toEqual([]);
    await bridge.resizeTerminal(B, 100, 40);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("desktop_terminal_resize", { sessionId: "session-b", cols: 100, rows: 40 }));
    await bridge.dispose();
  });

  /** Scenario: the successor's attach answers with an applied geometry, and only afterwards does a tile subscribe to geometry. The replay of the cached geometry names the process the session was attached on, as a live push does; the subscription made before the attach (control) received the same five values live. */
  it("replays a cached geometry with the process its session was attached on", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_bootstrap") return snapshot;
      if (command === "desktop_terminal_attach") return { sessionId: "session-b", agentId: "agent-1", generation: 2, reused: false, appliedRows: 48, appliedCols: 160 };
      return { ok: true };
    });
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    const live = vi.fn();
    bridge.onTerminalGeometry(live);
    await bridge.setShownTerminals([B]);
    await vi.waitFor(() => expect(live).toHaveBeenCalledWith("agent-1", 48, 160, DECK, "process-b"));

    const late = vi.fn();
    bridge.onTerminalGeometry(late);
    expect(late.mock.calls).toEqual([["agent-1", 48, 160, DECK, "process-b"]]);
    await bridge.dispose();
  });

  /** Scenario: terminal attaches fail three ways. A restart refusal and an unidentified-deck refusal each keep their own sentence, in the TUI's words; any other failure says only that the attach failed and asserts nothing about whether the agent is running. */
  it("words an attach failure without claiming the agent is running", async () => {
    const { terminalAttachFailure, AGENT_GONE_AFTER_RESTART, AGENT_IDENTITY_UNKNOWN } = await import("./bridge");
    expect(terminalAttachFailure(new Error(AGENT_GONE_AFTER_RESTART))).toBe("that agent is gone (the deck restarted)");
    expect(terminalAttachFailure(new Error(AGENT_IDENTITY_UNKNOWN))).toBe("the deck did not identify itself, so the action on that agent was not sent");
    const generic = terminalAttachFailure(new Error("connection reset"));
    expect(generic).toBe("Terminal attach failed; reconnect to retry its terminal.");
    expect(generic).not.toMatch(/still running/);
  });
});
