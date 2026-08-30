import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopAgentDto, DesktopSnapshotDto, TerminalAttachResult } from "./bridge";

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

const snapshot: DesktopSnapshotDto = {
  connection: {
    status: "connected",
    socketPath: "/tmp/deck.sock",
    clientProtocolVersion: 6,
    serverProtocolVersion: 6,
    clientBuildVersion: "0.1.0",
    daemonBuildVersion: "0.1.0",
    runningAgentCount: 1,
  },
  agents: [{
    id: "agent-1",
    paneId: "pane-1",
    displayName: "Builder",
    cwd: "/tmp/project",
    rows: 32,
    cols: 120,
    agentType: "codex",
    status: "working",
    activeTool: { name: "apply_patch", detail: "desktop/src/App.tsx" },
    toolCount: 4,
    tab: { kind: "orchestration", name: "dot-agent-deck", roleIndex: 1, roleName: "coder", isStartRole: false, displayTitle: "dot-agent-deck" },
  }],
  protocolVersion: 6,
  source: "daemon",
};

describe("TauriDeckBridge", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.clear();
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_bootstrap") return snapshot;
      if (command === "desktop_terminal_attach") return { sessionId: "session-7", agentId: "agent-1", generation: 7, reused: false };
      return { ok: true };
    });
  });

  afterEach(() => vi.restoreAllMocks());

  it("maps nullable daemon metadata honestly and uses session IDs for terminal RPCs", async () => {
    const { TauriDeckBridge, mapDesktopSnapshot } = await import("./bridge");
    const incompatible = structuredClone(snapshot);
    incompatible.connection.status = "incompatible";
    incompatible.connection.error = "protocol mismatch";
    delete incompatible.agents[0].cwd;
    delete incompatible.agents[0].displayName;
    delete incompatible.agents[0].activeTool;
    expect(mapDesktopSnapshot(incompatible)).toMatchObject({
      health: "failed",
      connection: { status: "error", daemonDetected: true, runningAgentCount: 1 },
      agents: [{ displayName: "Coder", cwd: "Unavailable", model: "Unavailable", task: "Task metadata unavailable from daemon" }],
    });

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    const view = await bridge.connect();

    expect(view.agents[0]).toMatchObject({ role: "Coder", model: "Unavailable", duration: "—", writeLease: "unknown", activeTool: "apply_patch" });
    const attachCall = invoke.mock.calls.find(([command]) => command === "desktop_terminal_attach");
    expect(attachCall?.[1]).toMatchObject({ agentId: "agent-1" });
    const channel = attachCall?.[1].onOutput as MockChannel<ArrayBuffer>;
    channel.onmessage?.(new Uint8Array([65, 66]).buffer);
    expect(output).toHaveBeenCalledWith(expect.objectContaining({ agentId: "agent-1", data: new Uint8Array([65, 66]) }));

    await bridge.sendTerminalInput("agent-1", "x");
    expect(invoke).toHaveBeenCalledWith("desktop_terminal_write", { sessionId: "session-7", data: [120] });

    await bridge.resizeTerminal("agent-1", 130, 35);
    await bridge.resizeTerminal("agent-1", 140, 40);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("desktop_terminal_resize", { sessionId: "session-7", cols: 140, rows: 40 }));
    expect(invoke).not.toHaveBeenCalledWith("desktop_terminal_resize", { sessionId: "session-7", cols: 130, rows: 35 });
    await bridge.dispose();
    expect(invoke).toHaveBeenCalledWith("desktop_terminal_detach", { sessionId: "session-7" });
  });

  it("clears sessions synchronously so StrictMode replay can reattach while detach is pending", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();

    let releaseDetach!: () => void;
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_terminal_detach") return new Promise<void>((resolve) => { releaseDetach = resolve; });
      if (command === "desktop_bootstrap") return snapshot;
      if (command === "desktop_terminal_attach") return { sessionId: "session-8", agentId: "agent-1", generation: 8, reused: false };
      return { ok: true };
    });

    const disposing = bridge.dispose();
    const reconnecting = bridge.connect();
    await reconnecting;
    expect(invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach")).toHaveLength(2);
    releaseDetach();
    await disposing;
  });

  it("surfaces a sanitized per-agent notice without failing the snapshot when terminal attach fails", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_bootstrap") return snapshot;
      if (command === "desktop_terminal_attach") throw new Error("secret token at /Users/private/project");
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    await expect(bridge.connect()).resolves.toMatchObject({ agents: [{ id: "agent-1" }] });

    const terminal = vi.fn();
    await bridge.subscribe(vi.fn(), terminal);
    expect(terminal).toHaveBeenCalledWith({
      agentId: "agent-1",
      data: new Uint8Array(),
      stream: "error",
      operation: "append",
      message: "Terminal attach failed. The agent is still running; reconnect to retry its terminal.",
    });
    expect(JSON.stringify(terminal.mock.calls)).not.toContain("secret token");
    expect(JSON.stringify(terminal.mock.calls)).not.toContain("/Users/private");
    await bridge.dispose();
  });

  it("replaces replay on in-process reattach and drops stale generation output and state", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    let attachGeneration = 7;
    let resolveSecondAttach!: (result: TerminalAttachResult) => void;
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_bootstrap") return snapshot;
      if (command === "desktop_terminal_attach") {
        if (attachGeneration === 8) {
          return new Promise<TerminalAttachResult>((resolve) => { resolveSecondAttach = resolve; });
        }
        return {
          sessionId: `session-${attachGeneration}`,
          agentId: "agent-1",
          generation: attachGeneration,
          reused: false,
        };
      }
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();

    const firstAttach = invoke.mock.calls.find(([command]) => command === "desktop_terminal_attach");
    const firstChannel = firstAttach?.[1].onOutput as MockChannel<ArrayBuffer>;
    firstChannel.onmessage?.(new TextEncoder().encode("old\r\n").buffer);

    listeners.get("desktop://terminal-state")?.({
      payload: {
        sessionId: "session-7",
        agentId: "agent-1",
        generation: 7,
        state: "end",
      },
    });

    attachGeneration = 8;
    const reconnecting = bridge.connect();
    await vi.waitFor(() => {
      expect(invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach")).toHaveLength(2);
    });
    const attachCalls = invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach");
    const secondChannel = attachCalls[1][1].onOutput as MockChannel<ArrayBuffer>;
    secondChannel.onmessage?.(new TextEncoder().encode("old\r\nnew\r\n").buffer);
    resolveSecondAttach({ sessionId: "session-8", agentId: "agent-1", generation: 8, reused: false });
    await reconnecting;

    const callsBeforeStaleEvents = output.mock.calls.length;
    firstChannel.onmessage?.(new TextEncoder().encode("stale-channel").buffer);
    listeners.get("desktop://terminal-state")?.({
      payload: {
        sessionId: "session-7",
        agentId: "agent-1",
        generation: 7,
        state: "error",
        message: "stale-state",
      },
    });
    expect(output).toHaveBeenCalledTimes(callsBeforeStaleEvents);

    const generationEight = output.mock.calls
      .map(([event]) => event)
      .filter((event) => event.generation === 8);
    expect(generationEight).toHaveLength(1);
    expect(generationEight[0]).toMatchObject({ operation: "replace", generation: 8 });
    expect(Array.from(generationEight[0].data)).toEqual(Array.from(new TextEncoder().encode("old\r\nnew\r\n")));

    secondChannel.onmessage?.(new TextEncoder().encode("live\r\n").buffer);
    const liveEvent = output.mock.calls.at(-1)?.[0];
    expect(liveEvent).toMatchObject({ generation: 8, operation: "append" });
    expect(Array.from(liveEvent.data)).toEqual(Array.from(new TextEncoder().encode("live\r\n")));
    await bridge.dispose();
  });

  it("rejects an explicit daemon start unless bootstrap returns connected", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const disconnected = structuredClone(snapshot);
    disconnected.connection.status = "disconnected";
    disconnected.connection.error = "daemon start timed out";
    disconnected.agents = [];
    invoke.mockResolvedValue(disconnected);

    const bridge = new TauriDeckBridge();
    await expect(bridge.runAction({ type: "start_daemon" })).rejects.toThrow("daemon start timed out");
    await bridge.dispose();
  });

  it("uses the desktop project cwd when no daemon agents are active", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const empty = structuredClone(snapshot);
    empty.agents = [];
    empty.projectCwd = "/Users/prabhusriramulu/dev/active/dot-agent-deck-gui";

    expect(mapDesktopSnapshot(empty)).toMatchObject({
      repo: "dot-agent-deck-gui",
      worktree: "/Users/prabhusriramulu/dev/active/dot-agent-deck-gui",
    });
  });

  it("sends stop_daemon through the live bridge", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();

    await bridge.runAction({ type: "stop_daemon" });

    expect(invoke).toHaveBeenCalledWith("desktop_run_action", { action: { type: "stop_daemon" } });
    await bridge.dispose();
  });

  it("sends restart_daemon through the live bridge", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();

    await bridge.runAction({ type: "restart_daemon" });

    expect(invoke).toHaveBeenCalledWith("desktop_run_action", { action: { type: "restart_daemon" } });
    await bridge.dispose();
  });

  /**
   * A cross-layer pin, not a table lookup. `DesktopAgentDto["status"]` mirrors
   * the Rust union, and `map_agent` emits `"running"` for an agent whose hook
   * state has not arrived yet — `record_without_hook_state_is_still_running`
   * in `desktop/src-tauri/src/dto.rs` pins exactly that. `DAEMON_STATUS` had no
   * `running` key, so that agent fell through the unknown-status default and a
   * live agent the daemon called running was labelled "waiting" on every screen
   * that reads status. Every member of the union is asserted, so the next
   * addition on the Rust side has to be answered here rather than silently
   * absorbed by the fallthrough.
   */
  it("maps every status the desktop DTO declares, including the hookless `running`", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const expected: Record<DesktopAgentDto["status"], string> = {
      running: "running",
      thinking: "running",
      working: "running",
      compacting: "running",
      waiting_for_input: "waiting",
      idle: "waiting",
      error: "failed",
      unknown: "waiting",
    };

    for (const [daemonStatus, deckStatus] of Object.entries(expected)) {
      const dto = structuredClone(snapshot);
      dto.agents[0].status = daemonStatus as DesktopAgentDto["status"];
      expect(mapDesktopSnapshot(dto).agents[0]?.status, `daemon status "${daemonStatus}"`).toBe(deckStatus);
    }

    // The fallthrough itself stays "waiting" for a status this build has never
    // heard of — a newer daemon may add one without a protocol bump.
    const future = structuredClone(snapshot);
    future.agents[0].status = "hyperthinking" as DesktopAgentDto["status"];
    expect(mapDesktopSnapshot(future).agents[0]?.status).toBe("waiting");
  });

  it("carries the daemon identity and the daemon's own tab membership onto the agent model", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");

    const mapped = mapDesktopSnapshot(structuredClone(snapshot));

    // The socket path is the only per-daemon identity the handshake reports,
    // and agent ids are per-daemon integers — so nothing may key on `id` alone.
    expect(mapped.agents[0]).toMatchObject({
      daemonId: "/tmp/deck.sock",
      activeTool: "apply_patch",
      activeToolDetail: "desktop/src/App.tsx",
      tab: { kind: "orchestration", name: "dot-agent-deck", roleIndex: 1, roleName: "coder", isStartRole: false, displayTitle: "dot-agent-deck" },
    });
  });
});

describe("FixtureDeckBridge scenarios", () => {
  const search = window.location.search;
  afterEach(() => window.history.replaceState({}, "", `/${search}`));

  it("reaches the crowded scenario from ?state=crowded", async () => {
    window.history.replaceState({}, "", "/?fixture=1&state=crowded");
    const { createDeckBridge } = await import("./bridge");

    const view = await createDeckBridge("fixture").connect();

    expect(view.agents).toHaveLength(15);
    expect(view.connection.status).toBe("connected");
    expect(new Set(view.agents.map((agent) => agent.tab.kind))).toEqual(new Set(["orchestration", "mode", "dashboard"]));
  });

  it("treats ?state=empty as a healthy daemon owning nothing, not as a disconnected one", async () => {
    window.history.replaceState({}, "", "/?fixture=1&state=empty");
    const { createDeckBridge } = await import("./bridge");

    const view = await createDeckBridge("fixture").connect();

    expect(view.connection.status).toBe("connected");
    expect(view.agents).toHaveLength(0);
  });

  it("falls back to the four-agent scenario for an unknown ?state=", async () => {
    window.history.replaceState({}, "", "/?fixture=1&state=nonsense");
    const { createDeckBridge } = await import("./bridge");

    const view = await createDeckBridge("fixture").connect();

    expect(view.agents).toHaveLength(4);
    expect(view.connection.status).toBe("connected");
  });
});
