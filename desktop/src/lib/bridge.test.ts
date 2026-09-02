import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DesktopSnapshotDto, TerminalAttachResult } from "./bridge";

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
});

describe("desktop settings (PRD 803)", () => {
  beforeEach(() => {
    invoke.mockReset();
    window.localStorage.clear();
  });

  afterEach(() => vi.restoreAllMocks());

  it("reads and writes the Rust-owned document through the live bridge", async () => {
    const { TauriDeckBridge, DEFAULT_DESKTOP_SETTINGS } = await import("./bridge");
    const stored = { version: 1, appearance: { mode: "dark" as const } };
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_get_settings") return DEFAULT_DESKTOP_SETTINGS;
      if (command === "desktop_set_settings") return stored;
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    expect(await bridge.getSettings()).toEqual(DEFAULT_DESKTOP_SETTINGS);
    expect(invoke).toHaveBeenCalledWith("desktop_get_settings");

    expect(await bridge.saveSettings(stored)).toEqual(stored);
    expect(invoke).toHaveBeenCalledWith("desktop_set_settings", { settings: stored });
    await bridge.dispose();
  });

  it("falls back to defaults when the settings IPC fails, but surfaces a failed save", async () => {
    const { TauriDeckBridge, DEFAULT_DESKTOP_SETTINGS } = await import("./bridge");
    invoke.mockImplementation(async (command: string) => {
      if (command === "desktop_get_settings" || command === "desktop_set_settings") {
        throw new Error("could not write the desktop settings file: Permission denied");
      }
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    expect(await bridge.getSettings()).toEqual(DEFAULT_DESKTOP_SETTINGS);
    await expect(bridge.saveSettings(DEFAULT_DESKTOP_SETTINGS)).rejects.toThrow("Permission denied");
    await bridge.dispose();
  });

  it("keeps fixture settings in unscoped localStorage and never invokes Tauri", async () => {
    const { createDeckBridge, DEFAULT_DESKTOP_SETTINGS, FIXTURE_SETTINGS_KEY, modeScopedKey } = await import("./bridge");
    const bridge = createDeckBridge("fixture");

    expect(await bridge.getSettings()).toEqual(DEFAULT_DESKTOP_SETTINGS);
    const light = { version: 1, appearance: { mode: "light" as const } };
    expect(await bridge.saveSettings(light)).toEqual(light);

    // A theme choice is global: the key must NOT carry the `.fixture`/`.live`
    // suffix every project-draft key does.
    expect(window.localStorage.getItem(FIXTURE_SETTINGS_KEY)).toBe(JSON.stringify(light));
    expect(FIXTURE_SETTINGS_KEY).not.toBe(modeScopedKey(FIXTURE_SETTINGS_KEY));
    expect(window.localStorage.getItem(modeScopedKey(FIXTURE_SETTINGS_KEY))).toBeNull();

    // The fixture preview can never reach the real desktop.toml: it holds no
    // Tauri handle at all.
    expect(invoke).not.toHaveBeenCalled();

    // A fresh bridge (a page reload) reads the choice back.
    expect(await createDeckBridge("fixture").getSettings()).toEqual(light);
    await bridge.dispose();
  });

  it("coerces an unreadable stored document back to defaults", async () => {
    const { createDeckBridge, DEFAULT_DESKTOP_SETTINGS, FIXTURE_SETTINGS_KEY, normalizeDesktopSettings } = await import("./bridge");
    window.localStorage.setItem(FIXTURE_SETTINGS_KEY, "{not json");
    expect(await createDeckBridge("fixture").getSettings()).toEqual(DEFAULT_DESKTOP_SETTINGS);

    window.localStorage.setItem(FIXTURE_SETTINGS_KEY, JSON.stringify({ version: 9, appearance: { mode: "solarized" } }));
    expect(await createDeckBridge("fixture").getSettings()).toEqual({ version: 9, appearance: { mode: "system" } });

    expect(normalizeDesktopSettings(undefined)).toEqual(DEFAULT_DESKTOP_SETTINGS);
    expect(normalizeDesktopSettings({ appearance: { mode: "dark" } })).toEqual({ version: 1, appearance: { mode: "dark" } });
  });
});
