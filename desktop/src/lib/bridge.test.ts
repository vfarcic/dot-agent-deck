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
    const mappedIncompatible = mapDesktopSnapshot(incompatible);
    expect(mappedIncompatible).toMatchObject({
      health: "failed",
      connection: { status: "error", daemonDetected: true, runningAgentCount: 1 },
      agents: [{ displayName: "Coder", model: "Unavailable", task: "Task metadata unavailable from daemon" }],
    });
    // An unreported cwd is ABSENT on the model, not the deck's stand-in word:
    // that word is a directory name the daemon can legitimately report, so a
    // sentinel spelled in it is one an agent can forge (M8 audit).
    expect(mappedIncompatible.agents[0]?.cwd).toBeUndefined();

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    const view = await bridge.connect();
    // PRD #745 M7: attach is demand-driven, so a test that needs a session has
    // to declare the terminal shown. This test is about the DTO mapping and the
    // session-id RPCs, not about what triggers an attach.
    await bridge.setShownTerminals(["agent-1"]);

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
    // PRD #745 M7: the session under test is the one the shown terminal asks
    // for. The property is unchanged — `dispose()` clears `sessions`
    // synchronously, so the reattach below goes through while its detach is
    // still pending — only the trigger moved off `connect()`.
    await bridge.setShownTerminals(["agent-1"]);

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
    await bridge.setShownTerminals(["agent-1"]);
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
    // PRD #745 M7: the attach that fails is the one the shown terminal asked
    // for, and asking must not surface the failure as a rejection either.
    await expect(bridge.setShownTerminals(["agent-1"])).resolves.toBeUndefined();

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
    // PRD #745 M7: demand-driven attach. Nothing else about this test changes —
    // it is the only coverage of the generation guards.
    await bridge.setShownTerminals(["agent-1"]);

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
    // The `end` event above dropped agent-1's session while the terminal stayed
    // on screen. Re-declaring the unchanged shown set is what brings it back —
    // `connect()` no longer attaches anything of its own.
    const reattaching = bridge.setShownTerminals(["agent-1"]);
    await vi.waitFor(() => {
      expect(invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach")).toHaveLength(2);
    });
    const attachCalls = invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach");
    const secondChannel = attachCalls[1][1].onOutput as MockChannel<ArrayBuffer>;
    secondChannel.onmessage?.(new TextEncoder().encode("old\r\nnew\r\n").buffer);
    resolveSecondAttach({ sessionId: "session-8", agentId: "agent-1", generation: 8, reused: false });
    await Promise.all([reconnecting, reattaching]);

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

  it("sends allow_build_mismatch through the live bridge", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();

    await bridge.runAction({ type: "allow_build_mismatch" });

    expect(invoke).toHaveBeenCalledWith("desktop_run_action", { action: { type: "allow_build_mismatch" } });
    await bridge.dispose();
  });

  /**
   * Issue #801. The webview cannot tell a stamp mismatch from a protocol
   * mismatch by reading the message, so the crate says which it is and the flag
   * has to survive the mapping — every Connect anyway affordance is gated on it.
   */
  it("carries the stamp-only mismatch flag through to the connection view", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");

    const stampOnly = structuredClone(snapshot);
    stampOnly.connection.status = "incompatible";
    stampOnly.connection.error = "build mismatch: desktop is a, daemon is b. Connect anyway to keep this one.";
    stampOnly.connection.buildStampMismatchOnly = true;
    expect(mapDesktopSnapshot(stampOnly).connection).toMatchObject({ status: "error", buildStampMismatchOnly: true });

    const protocolMismatch = structuredClone(snapshot);
    protocolMismatch.connection.status = "incompatible";
    protocolMismatch.connection.error = "protocol mismatch: desktop expects 8, daemon reports 7";
    protocolMismatch.connection.buildStampMismatchOnly = false;
    expect(mapDesktopSnapshot(protocolMismatch).connection.buildStampMismatchOnly).toBe(false);
  });

  /**
   * Issue #801. The new ordinary case: two builds from different commits that
   * name the SAME release. The crate connects with no error at all, so nothing
   * downstream may invent one — no override flag, and the healthy fallback
   * message rather than a mismatch note. Both stamps still ride along so the
   * difference stays discoverable on hover.
   */
  it("maps a same-release stamp difference as an ordinary healthy connection", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const sameRelease = structuredClone(snapshot);
    sameRelease.connection.status = "connected";
    sameRelease.connection.clientBuildVersion = "0.39.0-49-ga0165f8";
    sameRelease.connection.daemonBuildVersion = "0.39.0-g1ea0fe7";
    sameRelease.connection.buildStampMismatchOnly = false;
    delete sameRelease.connection.error;

    const mapped = mapDesktopSnapshot(sameRelease);

    expect(mapped.connection).toMatchObject({
      status: "connected",
      message: "Daemon responding",
      buildStampMismatchOnly: false,
      clientBuildVersion: "0.39.0-49-ga0165f8",
      daemonBuildVersion: "0.39.0-g1ea0fe7",
    });
    expect(mapped.connection.message).not.toContain("mismatch");
    expect(mapped.health).toBe("healthy");
  });

  /**
   * The whole point of the override: connected, and STILL saying so. The crate
   * keeps the mismatch in `error` on the bypass path, and this mapping is what
   * would drop it — a `connected` status used to be enough to reach for the
   * "Daemon responding" fallback, which would have made the caveat invisible
   * the moment it mattered.
   */
  it("keeps the build-mismatch caveat visible after connecting anyway", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const overridden = structuredClone(snapshot);
    overridden.connection.status = "connected";
    overridden.connection.daemonBuildVersion = "v0.39.0";
    overridden.connection.buildStampMismatchOnly = true;
    overridden.connection.error = "build mismatch: desktop is v0.38.0-50-gf118e99, daemon is v0.39.0. Connected anyway for this session; protocol 8 matched on both sides.";

    const mapped = mapDesktopSnapshot(overridden);

    expect(mapped.connection.status).toBe("connected");
    expect(mapped.connection.message).toContain("build mismatch");
    expect(mapped.connection.message).toContain("Connected anyway for this session");
    expect(mapped.health).toBe("healthy");
  });

  /**
   * The fallback the crate never actually triggers today. It used to hardcode
   * `Protocol mismatch` for every incompatible status, so the first caller to
   * make it reachable would have been told to compare protocol versions that
   * matched (issue #801).
   */
  it("names the check that actually failed when the crate sent no message", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");

    const stampOnly = structuredClone(snapshot);
    stampOnly.connection.status = "incompatible";
    stampOnly.connection.clientBuildVersion = "v0.38.0-50-gf118e99";
    stampOnly.connection.daemonBuildVersion = "v0.39.0";
    stampOnly.connection.buildStampMismatchOnly = true;
    delete stampOnly.connection.error;
    expect(mapDesktopSnapshot(stampOnly).connection.message).toBe("Build mismatch: desktop is v0.38.0-50-gf118e99, daemon is v0.39.0.");

    const protocolMismatch = structuredClone(snapshot);
    protocolMismatch.connection.status = "incompatible";
    protocolMismatch.connection.serverProtocolVersion = 7;
    delete protocolMismatch.connection.error;
    expect(mapDesktopSnapshot(protocolMismatch).connection.message).toBe("Protocol mismatch: desktop v6, daemon v7");
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

  /**
   * PRD #745 M8. The three fields the daemon has always sent and the desktop
   * has always dropped: the last user prompt (which also replaces the
   * hardcoded task placeholder), the write lease, and the orchestration tab's
   * own cwd.
   */
  it("carries the daemon's last prompt, write lease and orchestration cwd onto the agent model", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const reported = structuredClone(snapshot);
    reported.agents[0].lastUserPrompt = "Surface the honest fields.";
    reported.agents[0].writeLease = "read";
    reported.agents[0].tab = { kind: "orchestration", name: "dot-agent-deck", roleIndex: 1, roleName: "coder", isStartRole: false, displayTitle: "dot-agent-deck", cwd: "/work/deck" };

    const mapped = mapDesktopSnapshot(reported);

    expect(mapped.agents[0]).toMatchObject({
      lastUserPrompt: "Surface the honest fields.",
      writeLease: "read",
      // The prompt is the honest answer to "what was this asked to do", so it
      // leads the deck's assignment line ahead of the active-tool restatement.
      task: "Surface the honest fields.",
      tab: { kind: "orchestration", cwd: "/work/deck" },
    });
  });

  /**
   * The absent case for all three. Absence stays absent — `"unknown"` is the
   * lease's sentinel, reversed at the overview's own boundary — and the task
   * line falls back to the active tool exactly as it always did.
   */
  it("reports no prompt, no lease and no orchestration cwd when the daemon sent none", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");

    const mapped = mapDesktopSnapshot(structuredClone(snapshot));

    expect(mapped.agents[0]?.lastUserPrompt).toBeUndefined();
    expect(mapped.agents[0]?.writeLease).toBe("unknown");
    expect(mapped.agents[0]?.task).toBe("Active tool: apply_patch · desktop/src/App.tsx");
    expect(mapped.agents[0]?.tab).toMatchObject({ kind: "orchestration" });
    expect((mapped.agents[0]?.tab as { cwd?: string }).cwd).toBeUndefined();
  });

  /**
   * PRD #745 M9. The daemon's `last_activity_ms` reaches the agent model as the
   * same integer — no reformatting, no relative wording, no sentinel — because
   * only the render seam knows what "now" is and only it can weigh the two
   * clocks. Absence travels as absence, which is what an older daemon and a
   * restarted one both produce.
   */
  it("carries the daemon's last-activity instant through unchanged, and absence as absence", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const reported = structuredClone(snapshot);
    reported.agents[0].lastActivityMs = 1_756_684_800_123;

    expect(mapDesktopSnapshot(reported).agents[0]?.lastActivityMs).toBe(1_756_684_800_123);
    expect(mapDesktopSnapshot(structuredClone(snapshot)).agents[0]?.lastActivityMs).toBeUndefined();
  });

  /**
   * PRD #745 M11, the same contract for the spawn instant: the same integer
   * through to the agent model, and absence as absence — which is what a daemon
   * that did not spawn the agent, and one predating the field, both produce.
   */
  it("carries the daemon's spawn instant through unchanged, and absence as absence", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const reported = structuredClone(snapshot);
    reported.agents[0].spawnedAtMs = 1_756_684_800_123;

    expect(mapDesktopSnapshot(reported).agents[0]?.spawnedAtMs).toBe(1_756_684_800_123);
    expect(mapDesktopSnapshot(structuredClone(snapshot)).agents[0]?.spawnedAtMs).toBeUndefined();
  });

  /**
   * PRD #745. `cli` is the BINARY the agent runs, resolved daemon-side from the
   * agent registry, and never the wire identity beside it: rendering
   * `agentType` printed `claude_code` and `open_code`, neither of which anybody
   * types. Where this build cannot name a binary — the daemon reported `none`,
   * which is also where an agent type from a NEWER daemon lands — the deck's
   * own generic word stands in rather than an invented one.
   */
  it("renders the CLI binary the daemon resolved, and never the agent-type enum", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const claude = structuredClone(snapshot);
    claude.agents[0].agentType = "claude_code";
    claude.agents[0].cliName = "claude";
    // Outside an orchestration the deck's role label is derived from the wire
    // identity, which this leaves untouched.
    claude.agents[0].tab = { kind: "dashboard" };

    expect(mapDesktopSnapshot(claude).agents[0]?.cli).toBe("claude");
    expect(mapDesktopSnapshot(claude).agents[0]?.role).toBe("Claude code");

    const unnameable = structuredClone(snapshot);
    unnameable.agents[0].agentType = "none";
    expect(mapDesktopSnapshot(unnameable).agents[0]?.cli).toBe("agent");
  });

  /**
   * The M8 audit's cwd finding. `src/agent_pty.rs` accepts any non-empty,
   * bounded, control-free working directory, so `"Unavailable"` — the deck's
   * own stand-in word — is a directory an agent can genuinely be launched in.
   * While the bridge wrote that word for ABSENCE, such an agent had its real,
   * reported directory erased at the overview's boundary into a blank cell with
   * no hover text. The reported value now survives, and it is a candidate for
   * the snapshot's repo directory like any other.
   */
  it("does not erase a reported working directory that spells the deck's stand-in word", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const collides = structuredClone(snapshot);
    collides.agents[0].cwd = "Unavailable";

    const mapped = mapDesktopSnapshot(collides);

    expect(mapped.agents[0]?.cwd).toBe("Unavailable");
    expect(mapped.worktree).toBe("Unavailable");
  });

  /**
   * The M8 audit's prompt finding, at the seam that closes it. The deck renders
   * `task` straight into a DOM text node, so the bridge — not the tile — makes
   * it a display copy: sanitised of controls and bidi overrides, and clamped to
   * the prompt budget, whatever the daemon sent. The active-tool restatement
   * goes through the same seam.
   */
  it("projects the assignment line as a bounded, sanitised display copy of the prompt", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");
    const { DISPLAY_LIMITS } = await import("./displayText");
    const hostile = structuredClone(snapshot);
    hostile.agents[0].lastUserPrompt = `\u202eEVIL\u0007${"p".repeat(70_000)}`;

    const mapped = mapDesktopSnapshot(hostile);

    const task = mapped.agents[0]?.task ?? "";
    expect(task).not.toContain("\u202e");
    expect(task).not.toContain("\u0007");
    expect(Array.from(task).length).toBe(DISPLAY_LIMITS.prompt + 1);
    // The RAW prompt stays on its own field: the overview applies its own
    // budgets to it, including the longer one for the hover copy.
    expect(mapped.agents[0]?.lastUserPrompt).toBe(hostile.agents[0].lastUserPrompt);

    // And the fallback branch is bounded too — a tool detail is the agent's own
    // command line, and was equally raw on this path before M8 touched it.
    const tooling = structuredClone(snapshot);
    tooling.agents[0].activeTool = { name: "bash", detail: `\u202erm -rf ${"x".repeat(1_000)}` };
    const toolTask = mapDesktopSnapshot(tooling).agents[0]?.task ?? "";
    expect(toolTask).not.toContain("\u202e");
    expect(Array.from(toolTask).length).toBe(DISPLAY_LIMITS.prompt + 1);
  });

  /**
   * PRD #745 M8. No daemon tracks a retry count or a per-agent git branch, so
   * live mode now reports neither instead of the `1` every tile printed as
   * `ATT 01` and the literal "Unavailable" the topbar printed as a branch.
   */
  it("fabricates no attempt count and no branch in live mode", async () => {
    const { mapDesktopSnapshot } = await import("./bridge");

    const mapped = mapDesktopSnapshot(structuredClone(snapshot));

    expect(mapped.agents[0]?.attempt).toBeUndefined();
    expect(mapped.stages[0]?.attempt).toBeUndefined();
    expect(mapped.currentAttempt).toBeUndefined();
    expect(mapped.branch).toBeUndefined();
    // And a previous snapshot cannot smuggle one back in: the field is not
    // carried forward, so a fixture-shaped value can never become live data.
    const carried = mapDesktopSnapshot(structuredClone(snapshot), { ...mapped, branch: "main", currentAttempt: 3 });
    expect(carried.branch).toBeUndefined();
    expect(carried.currentAttempt).toBeUndefined();
  });
});

describe("FixtureDeckBridge scenarios", () => {
  const search = window.location.search;
  afterEach(() => window.history.replaceState({}, "", `/${search}`));

  it("reaches the crowded scenario from ?state=crowded", async () => {
    window.history.replaceState({}, "", "/?fixture=1&state=crowded");
    const { createDeckBridge } = await import("./bridge");

    const bridge = createDeckBridge("fixture");
    const view = await bridge.connect();

    expect(view.agents).toHaveLength(15);
    expect(view.connection.status).toBe("connected");
    expect(new Set(view.agents.map((agent) => agent.tab.kind))).toEqual(new Set(["orchestration", "mode", "dashboard"]));

    // PRD #745 M7: the fixture preview drives the same screens as the live
    // bridge, so it has to answer the attach seam too — as a no-op, since it
    // owns no PTYs. Nothing in the UI may have to know which bridge it holds.
    await expect(bridge.setShownTerminals(view.agents.map((agent) => agent.id))).resolves.toBeUndefined();
    await expect(bridge.setShownTerminals([])).resolves.toBeUndefined();
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

/**
 * PRD #745 M7 — demand-driven attach.
 *
 * Rendering a terminal and attaching a PTY are unrelated today.
 * `TauriDeckBridge.attachAgents` attaches every agent in the snapshot that is
 * not already in its private `attached` set — never anything mounted — and it
 * fires from `connect()`, from every `desktop://snapshot` event, and from
 * `runAction`'s `start_daemon` branch. Each attach opens a real daemon
 * `AttachStream` socket, and an `AttachStream` replays the full scrollback
 * before it streams, so a screen that displays no output still costs one
 * socket and one replay per agent.
 *
 * These tests pin the replacement, whose shape is declarative on purpose:
 * `setShownTerminals(agentIds)` states the whole set of terminals currently on
 * screen, because the two facts the deck and the overview need cannot be
 * expressed by an imperative `showTerminal` at all — "nine are shown at once"
 * and "now none is".
 *
 * The decided semantics, which every test below pins some corner of:
 *
 * - Shown terminals are always attached, and shown terminals are NOT capped.
 *   Nine visible deck tiles means nine attaches, exactly as today; the deck
 *   must not change.
 * - Leaving a terminal does not detach it — it moves into a bounded *warm*
 *   set and stays attached, so coming back costs no scrollback replay.
 * - The warm set, and only the warm set, is bounded by `MAX_WARM_TERMINALS`
 *   (exported from `bridge.ts` so the bound the tests drive and the bound the
 *   implementation enforces cannot drift). Beyond it, evict least-recently-used
 *   with a full teardown.
 * - When the shown set becomes empty, the warm set flushes to zero — which is
 *   what makes "the overview attaches no PTYs" true even when you arrive at it
 *   from a nine-tile deck, rather than "zero new, up to three lingering".
 */
describe("TauriDeckBridge demand-driven attach (PRD #745 M7)", () => {
  /** The nine-agent fleet the PRD costs out: nine sockets, nine replays. */
  const FLEET_SIZE = 9;

  const fleetSnapshot = (count = FLEET_SIZE): DesktopSnapshotDto => {
    const dto = structuredClone(snapshot);
    dto.connection.runningAgentCount = count;
    dto.agents = Array.from({ length: count }, (_, index) => {
      const agent = structuredClone(snapshot.agents[0]);
      agent.id = `agent-${index + 1}`;
      agent.paneId = `pane-${index + 1}`;
      agent.displayName = `Agent ${index + 1}`;
      return agent;
    });
    return dto;
  };

  const fleetAgentIds = (count = FLEET_SIZE) => Array.from({ length: count }, (_, index) => `agent-${index + 1}`);

  const attachCalls = () => invoke.mock.calls.filter(([command]) => command === "desktop_terminal_attach");
  const attachedAgentIds = () => attachCalls().map(([, args]) => args.agentId as string);
  const detachCalls = () => invoke.mock.calls.filter(([command]) => command === "desktop_terminal_detach");
  const detachedSessionIds = () => detachCalls().map(([, args]) => args.sessionId as string);
  /**
   * Session ids embed the agent they belong to (`session-<agentId>-<gen>`), so
   * a detach can be attributed without predicting the generation — which is not
   * predictable when several attaches are in flight at once.
   */
  const detachedAgentIds = () => detachedSessionIds().map((sessionId) => /^session-(agent-\d+)-\d+$/.exec(sessionId)?.[1] ?? sessionId);

  /**
   * The eager path is fire-and-forget (`void this.attachAgents(...)`) and gets
   * to `invoke` after a handful of microtask ticks, so a *negative* assertion
   * has to outlive them or it passes for the wrong reason. A macrotask turn
   * drains the microtask queue; three of them leave no room for doubt.
   */
  const settle = async () => {
    for (let turn = 0; turn < 3; turn += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
  };

  let generation = 0;

  beforeEach(() => {
    invoke.mockReset();
    listeners.clear();
    generation = 0;
    invoke.mockImplementation(async (command: string, args?: { agentId?: string }) => {
      if (command === "desktop_bootstrap") return fleetSnapshot();
      if (command === "desktop_terminal_attach") {
        generation += 1;
        const agentId = args?.agentId ?? "";
        return { sessionId: `session-${agentId}-${generation}`, agentId, generation, reused: false } satisfies TerminalAttachResult;
      }
      return { ok: true };
    });
  });

  afterEach(() => vi.restoreAllMocks());

  /**
   * Installs the fleet mock with a switch that leaves one nominated agent's
   * `desktop_terminal_attach` unresolved, plus the function that finally
   * resolves it. Same in-flight trick the replay test uses inline, hoisted
   * because the two tests below both need an attach still pending when the
   * shown set changes underneath it. `hold` is armed *after* `connect()` so
   * that today's eager attach still resolves and the zero-attach baseline
   * fails on an assertion rather than hanging.
   */
  const holdableAttach = () => {
    let heldAgentId: string | undefined;
    let release: (() => void) | undefined;
    invoke.mockImplementation(async (command: string, args?: { agentId?: string }) => {
      if (command === "desktop_bootstrap") return fleetSnapshot();
      if (command === "desktop_terminal_attach") {
        generation += 1;
        const agentId = args?.agentId ?? "";
        const result: TerminalAttachResult = { sessionId: `session-${agentId}-${generation}`, agentId, generation, reused: false };
        if (agentId === heldAgentId) {
          return new Promise<TerminalAttachResult>((resolve) => { release = () => resolve(result); });
        }
        return result;
      }
      return { ok: true };
    });
    return {
      hold: (agentId: string) => { heldAgentId = agentId; },
      release: () => release?.(),
    };
  };

  /**
   * Scenario: bring the bridge up against a nine-agent daemon and push a
   * further snapshot at it, exactly as the overview does — subscribe, connect,
   * fire `desktop://snapshot` — while showing no terminal at all. Not one
   * `desktop_terminal_attach` may be invoked. This is the property the whole
   * PRD rests on: "shows no output" and "opens no PTYs" are different claims,
   * and only the first comes free.
   */
  it("attaches zero terminals for a nine-agent snapshot while no terminal is shown", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());

    const view = await bridge.connect();
    expect(view.agents).toHaveLength(FLEET_SIZE);

    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });
    await settle();

    expect(attachedAgentIds()).toEqual([]);
    await bridge.dispose();
  });

  /**
   * Scenario: connect against a nine-agent daemon and do nothing else — no
   * snapshot event, no terminal shown. `connect()` is one of the three eager
   * call sites and must attach nothing on its own.
   */
  it("attaches nothing from connect() alone", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());

    await bridge.connect();
    await settle();

    expect(attachedAgentIds()).toEqual([]);
    await bridge.dispose();
  });

  /**
   * Scenario: connect against a daemon owning no agents, then fire a
   * `desktop://snapshot` carrying nine brand-new ones. The snapshot listener is
   * another eager call site, and a fix that only addressed `connect()` would
   * leave it attaching the whole fleet — so it is asserted separately.
   */
  it("attaches nothing when a snapshot event brings new agents", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const empty = fleetSnapshot(0);
    invoke.mockImplementation(async (command: string, args?: { agentId?: string }) => {
      if (command === "desktop_bootstrap") return empty;
      if (command === "desktop_terminal_attach") {
        generation += 1;
        const agentId = args?.agentId ?? "";
        return { sessionId: `session-${agentId}-${generation}`, agentId, generation, reused: false } satisfies TerminalAttachResult;
      }
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    expect(attachedAgentIds()).toEqual([]);

    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });
    await settle();

    expect(attachedAgentIds()).toEqual([]);
    await bridge.dispose();
  });

  /**
   * Scenario: with nine agents in the snapshot and none attached, declare one
   * terminal shown. Exactly one attach is invoked and it names exactly that
   * agent — demand drives attach, and demand for one agent does not drag in its
   * eight neighbours.
   */
  it("attaches the shown agent, and only the shown agent", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-4"]);
    await settle();

    expect(attachedAgentIds()).toEqual(["agent-4"]);
    expect(detachCalls()).toHaveLength(0);
    await bridge.dispose();
  });

  /**
   * Scenario: read the exported warm bound and hold it to being a real bound —
   * at least 2, so bouncing between two agents you have left is free, and
   * strictly smaller than the nine-agent fleet, so it cannot quietly drift back
   * to holding everything warm. Every behavioural test below drives off this
   * constant rather than a literal, so raising it moves the tests with it.
   */
  it("exports the warm-set bound as a constant that genuinely bounds the fleet", async () => {
    const { MAX_WARM_TERMINALS } = await import("./bridge");

    expect(Number.isInteger(MAX_WARM_TERMINALS)).toBe(true);
    expect(MAX_WARM_TERMINALS).toBeGreaterThanOrEqual(2);
    expect(MAX_WARM_TERMINALS).toBeLessThan(FLEET_SIZE);
  });

  /**
   * Scenario: show agents one at a time, so each one shown pushes the previous
   * one into the warm set. Nothing is evicted while the warm set is merely full
   * (`MAX_WARM_TERMINALS` agents left behind, one still shown); the very next
   * agent shown overflows it and detaches the *least recently used* left
   * terminal — `agent-1` — rather than whichever one was convenient. The bound
   * caps only what you have left: one shown plus `MAX_WARM_TERMINALS` warm stay
   * attached throughout.
   */
  it("evicts the least recently used terminal once the warm bound is exceeded", async () => {
    const { TauriDeckBridge, MAX_WARM_TERMINALS } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    // Showing agent-k one at a time leaves agents 1..k-1 warm, so the warm set
    // is exactly full after MAX_WARM_TERMINALS + 1 agents and overflows on the
    // next one.
    const untilFull = fleetAgentIds(MAX_WARM_TERMINALS + 1);
    for (const agentId of untilFull) await bridge.setShownTerminals([agentId]);
    await settle();
    expect(attachedAgentIds()).toEqual(untilFull);
    expect(detachCalls()).toHaveLength(0);

    const overflowing = `agent-${MAX_WARM_TERMINALS + 2}`;
    await bridge.setShownTerminals([overflowing]);
    await settle();

    expect(attachedAgentIds()).toEqual([...untilFull, overflowing]);
    // One over the bound, so exactly one eviction, and it is the first left.
    expect(detachedSessionIds()).toEqual(["session-agent-1-1"]);
    // One shown plus a full warm set survive; the cap governs the warm set only.
    expect(attachCalls().length - detachCalls().length).toBe(MAX_WARM_TERMINALS + 1);

    // The evicted agent is genuinely gone, not merely detached on the daemon:
    // writing to it must fail rather than reach a dead session id.
    await expect(bridge.sendTerminalInput("agent-1", "x")).rejects.toThrow(/not attached/);
    await bridge.dispose();
  });

  /**
   * Scenario: show one terminal, show a second so the first goes warm, then go
   * back to the first — inside the bound. No second attach is invoked for it,
   * nothing is detached, and no fresh `replace` reaches the pane. This is the
   * entire reason the warm set exists: bouncing between agents must not pay a
   * scrollback replay each way.
   */
  it("does not re-attach a terminal revisited inside the bound", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-1"]);
    await bridge.setShownTerminals(["agent-2"]);
    const replacesBeforeRevisit = output.mock.calls.filter(([event]) => event.agentId === "agent-1" && event.operation === "replace").length;

    await bridge.setShownTerminals(["agent-1"]);
    await settle();

    expect(attachedAgentIds()).toEqual(["agent-1", "agent-2"]);
    expect(detachCalls()).toHaveLength(0);
    // A warm terminal comes back as it was left: no re-attach, so no replay.
    const replacesAfterRevisit = output.mock.calls.filter(([event]) => event.agentId === "agent-1" && event.operation === "replace").length;
    expect(replacesAfterRevisit).toBe(replacesBeforeRevisit);
    await bridge.dispose();
  });

  /**
   * Scenario: show all nine agents at once, the way the deck mounts a terminal
   * on every tile. All nine attach, **nothing** is detached, and every one of
   * them can still be written to — the warm bound governs terminals you have
   * left, never terminals on screen, so a three-deep bound must not kill six
   * visible panes. The PRD forbids altering the deck, and this is that
   * guarantee in test form.
   */
  it("never evicts a shown terminal, however many are shown at once", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(fleetAgentIds());
    await settle();

    expect(attachedAgentIds().sort()).toEqual(fleetAgentIds().sort());
    expect(detachCalls()).toHaveLength(0);
    // Not merely "no detach was invoked": every one still resolves to a live
    // session, so none was dropped client-side either.
    for (const agentId of fleetAgentIds()) {
      await expect(bridge.sendTerminalInput(agentId, "x")).resolves.toBeUndefined();
    }
    await bridge.dispose();
  });

  /**
   * Scenario: show all nine agents, then declare that none is shown — the
   * deck→overview transition. All nine detach and none survives as warm, so the
   * overview really does hold zero PTYs however you arrived at it. Nothing new
   * is attached on the way out.
   */
  it("flushes every warm terminal when the shown set becomes empty", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(fleetAgentIds());
    await settle();
    const attachesWhileShown = attachCalls().length;

    await bridge.setShownTerminals([]);
    await settle();

    expect(detachedAgentIds().sort()).toEqual(fleetAgentIds().sort());
    expect(attachCalls()).toHaveLength(attachesWhileShown);
    for (const agentId of fleetAgentIds()) {
      await expect(bridge.sendTerminalInput(agentId, "x")).rejects.toThrow(/not attached/);
    }
    await bridge.dispose();
  });

  /**
   * Scenario: show all nine agents but hold one agent's terminal attach
   * unresolved, declare that none is shown while that attach is still in
   * flight, and only then let it resolve. The late arrival must be detached
   * rather than installed — a flush has to cancel pending attaches as well as
   * tear down live ones, or the overview holds exactly one live PTY behind a
   * screen whose whole claim is that it holds none.
   */
  it("cancels an attach still in flight when the shown set becomes empty", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const heldAgentId = "agent-5";
    const attach = holdableAttach();
    const resolvedAgentIds = fleetAgentIds().filter((agentId) => agentId !== heldAgentId);

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    attach.hold(heldAgentId);
    const showing = bridge.setShownTerminals(fleetAgentIds());
    await vi.waitFor(() => expect(attachedAgentIds().sort()).toEqual(fleetAgentIds().sort()));
    await settle();

    // agent-5 is still mid-attach; the other eight are live.
    const flushing = bridge.setShownTerminals([]);
    // The flush must not block behind the pending attach — the eight that did
    // resolve tear down straight away.
    await vi.waitFor(() => expect(detachedAgentIds().sort()).toEqual(resolvedAgentIds.sort()));

    attach.release();
    await Promise.allSettled([showing, flushing]);
    await settle();

    // The late attach is detached, not installed.
    expect(detachedAgentIds().sort()).toEqual(fleetAgentIds().sort());
    expect(attachCalls()).toHaveLength(FLEET_SIZE);
    await expect(bridge.sendTerminalInput(heldAgentId, "x")).rejects.toThrow(/not attached/);
    // And its scrollback never lands in a pane that is no longer on screen.
    const replays = output.mock.calls.filter(([event]) => event.agentId === heldAgentId && event.operation === "replace");
    expect(replays).toHaveLength(0);
    await bridge.dispose();
  });

  /**
   * Scenario: show one agent but hold its terminal attach unresolved, then show
   * others one at a time until it is pushed out of the warm set, and only then
   * let its attach resolve. The same hazard on the narrower path: an eviction
   * that tears down installed sessions only finds nothing for an agent still
   * mid-attach, so that agent installs itself afterwards and the warm bound is
   * quietly exceeded by a terminal nobody is looking at.
   */
  it("cancels an attach still in flight when the agent is evicted from the warm set", async () => {
    const { TauriDeckBridge, MAX_WARM_TERMINALS } = await import("./bridge");
    const heldAgentId = "agent-1";
    const attach = holdableAttach();

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    attach.hold(heldAgentId);
    const showing = bridge.setShownTerminals([heldAgentId]);
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual([heldAgentId]));

    // Leaving it moves it into the warm set; MAX_WARM_TERMINALS + 1 further
    // agents shown one at a time overflow that set and make it the least
    // recently used — all while its first attach is still unresolved.
    for (let index = 2; index <= MAX_WARM_TERMINALS + 2; index += 1) {
      await bridge.setShownTerminals([`agent-${index}`]);
    }
    await settle();

    attach.release();
    await Promise.allSettled([showing]);
    await settle();

    expect(detachedAgentIds()).toEqual([heldAgentId]);
    await expect(bridge.sendTerminalInput(heldAgentId, "x")).rejects.toThrow(/not attached/);
    // The bound holds: one shown plus a full warm set, and the evicted agent
    // did not reinstate itself on the way out.
    expect(attachCalls()).toHaveLength(MAX_WARM_TERMINALS + 2);
    expect(attachCalls().length - detachCalls().length).toBe(MAX_WARM_TERMINALS + 1);
    const replays = output.mock.calls.filter(([event]) => event.agentId === heldAgentId && event.operation === "replace");
    expect(replays).toHaveLength(0);
    await bridge.dispose();
  });

  /**
   * Scenario: show an agent, let it stream output, push it out of the warm set
   * by showing others one at a time, then show it again while its second attach
   * is still in flight and let the daemon replay the whole scrollback. The
   * replay must reach the pane as a single `replace` at the new generation —
   * never an `append` — and the dead channel from the evicted attach must be
   * ignored, or an evict-and-return round trip duplicates every line the agent
   * has ever printed.
   */
  it("replays an evicted terminal as a replace, not a duplicate append", async () => {
    const { TauriDeckBridge, MAX_WARM_TERMINALS } = await import("./bridge");
    let releaseReattach!: () => void;
    let holdAgentId: string | undefined;
    invoke.mockImplementation(async (command: string, args?: { agentId?: string }) => {
      if (command === "desktop_bootstrap") return fleetSnapshot();
      if (command === "desktop_terminal_attach") {
        generation += 1;
        const agentId = args?.agentId ?? "";
        const result: TerminalAttachResult = { sessionId: `session-${agentId}-${generation}`, agentId, generation, reused: false };
        if (agentId === holdAgentId) {
          return new Promise<TerminalAttachResult>((resolve) => { releaseReattach = () => resolve(result); });
        }
        return result;
      }
      return { ok: true };
    });

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-1"]);
    const firstChannel = attachCalls()[0][1].onOutput as MockChannel<ArrayBuffer>;
    firstChannel.onmessage?.(new TextEncoder().encode("first\r\n").buffer);
    const firstGeneration = attachCalls().length;

    // Push agent-1 out of the warm set: one agent shown at a time, so agent-1
    // is the least recently used the moment the warm set overflows.
    for (let index = 2; index <= MAX_WARM_TERMINALS + 2; index += 1) {
      await bridge.setShownTerminals([`agent-${index}`]);
    }
    expect(detachedSessionIds()).toEqual([`session-agent-1-${firstGeneration}`]);

    // Come back to it. The daemon replays everything before it streams.
    holdAgentId = "agent-1";
    const reshowing = bridge.setShownTerminals(["agent-1"]);
    // agent-1 already appears once in the attach log from before its eviction,
    // so the wait has to be for a SECOND attach naming it, not for any.
    await vi.waitFor(() => expect(attachedAgentIds().filter((agentId) => agentId === "agent-1")).toHaveLength(2));
    const secondChannel = attachCalls().filter(([, args]) => args.agentId === "agent-1").at(-1)?.[1].onOutput as MockChannel<ArrayBuffer>;
    expect(secondChannel).not.toBe(firstChannel);
    secondChannel.onmessage?.(new TextEncoder().encode("first\r\nsecond\r\n").buffer);
    releaseReattach();
    await reshowing;

    const reattachGeneration = attachCalls().length;
    const replays = output.mock.calls
      .map(([event]) => event)
      .filter((event) => event.agentId === "agent-1" && event.generation === reattachGeneration);
    expect(replays).toHaveLength(1);
    expect(replays[0]).toMatchObject({ operation: "replace", generation: reattachGeneration });
    expect(new TextDecoder().decode(replays[0].data)).toBe("first\r\nsecond\r\n");

    // The evicted attach's channel is dead; anything it emits must be dropped.
    const callsBeforeStale = output.mock.calls.length;
    firstChannel.onmessage?.(new TextEncoder().encode("stale\r\n").buffer);
    expect(output).toHaveBeenCalledTimes(callsBeforeStale);

    // And writes now route through the NEW session, not the evicted one.
    await bridge.sendTerminalInput("agent-1", "x");
    expect(invoke).toHaveBeenCalledWith("desktop_terminal_write", { sessionId: `session-agent-1-${reattachGeneration}`, data: [120] });
    await bridge.dispose();
  });

  /**
   * Scenario: show one terminal, let its attach land, then have the daemon end
   * that session while the pane is still on screen — `handleTerminalState`
   * drops it, so input stops reaching the daemon — and fire a
   * `desktop://snapshot` event. The snapshot must re-assert the invariant
   * "everything currently shown is attached": a SECOND attach naming that
   * agent, a live session behind it, and a pane that takes input again.
   * Deleting the eager attach from the listener removed this healing by
   * accident, and `ControlDeck`'s effect cannot restore it — a dead session
   * does not change the derived shown set, so nothing re-fires until the user
   * toggles a tab.
   */
  it("re-attaches a shown terminal whose daemon session ended, on the next snapshot", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    const firstGeneration = attachCalls().length;
    expect(attachedAgentIds()).toEqual(["agent-1"]);
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();

    // The daemon ends the session under a terminal that is still on screen.
    listeners.get("desktop://terminal-state")?.({
      payload: {
        agentId: "agent-1",
        sessionId: `session-agent-1-${firstGeneration}`,
        generation: firstGeneration,
        state: "end",
      },
    });
    await expect(bridge.sendTerminalInput("agent-1", "x")).rejects.toThrow(/not attached/);

    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });

    // agent-1 already appears once in the attach log from before its session
    // ended, so the wait has to be for a SECOND attach naming it, not for any —
    // `toContain` would pass on the dead one and prove nothing.
    await vi.waitFor(() => expect(attachedAgentIds().filter((agentId) => agentId === "agent-1")).toHaveLength(2));
    await settle();

    // Usable again, and through the NEW session rather than the dead one.
    const reattachGeneration = attachCalls().length;
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();
    expect(invoke).toHaveBeenCalledWith("desktop_terminal_write", { sessionId: `session-agent-1-${reattachGeneration}`, data: [120] });
    // Healing the dead pane must not drag in the eight nobody is looking at.
    expect(attachedAgentIds()).toEqual(["agent-1", "agent-1"]);
    await bridge.dispose();
  });

  /**
   * Scenario: with one of nine agents shown and its session perfectly healthy,
   * fire a `desktop://snapshot` event. Re-asserting the invariant re-declares
   * the SHOWN set and nothing else, so a healthy snapshot costs no attach at
   * all and the other eight agents stay unattached. The counterpart to the test
   * above: healing a dead pane on every snapshot must not become the eager
   * fleet attach this PRD deleted, by the back door.
   */
  it("re-declares only the shown set on a snapshot event, never the fleet", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    const attachesWhileShown = attachCalls().length;
    expect(attachedAgentIds()).toEqual(["agent-1"]);

    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });
    await settle();

    expect(attachCalls()).toHaveLength(attachesWhileShown);
    expect(attachedAgentIds()).toEqual(["agent-1"]);
    expect(detachCalls()).toHaveLength(0);
    // Not merely "no attach was invoked": the eight have no session either.
    for (const agentId of fleetAgentIds().filter((agentId) => agentId !== "agent-1")) {
      await expect(bridge.sendTerminalInput(agentId, "x")).rejects.toThrow(/not attached/);
    }
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();
    await bridge.dispose();
  });


  /**
   * Scenario: show one agent, then show another so the first goes warm — left
   * behind but still attached — and have the daemon end the warm one's session
   * while nobody is looking at it. A `desktop://snapshot` must NOT heal it. The
   * invariant a snapshot re-asserts is "everything SHOWN is attached", never
   * "everything attached is alive": healing off screen would spend a socket and
   * a full scrollback replay on a pane nobody can see, and re-declaring the
   * snapshot's own agents instead of the shown set is exactly how that creeps
   * back in. Showing it again is what brings it back (PRD #745 M7).
   */
  it("leaves a warm terminal whose session ended dead until it is shown again", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    await bridge.setShownTerminals(["agent-1"]);
    const warmGeneration = attachCalls().length;
    await bridge.setShownTerminals(["agent-2"]);
    await settle();
    expect(attachedAgentIds()).toEqual(["agent-1", "agent-2"]);

    listeners.get("desktop://terminal-state")?.({
      payload: {
        agentId: "agent-1",
        sessionId: `session-agent-1-${warmGeneration}`,
        generation: warmGeneration,
        state: "end",
      },
    });
    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });
    await settle();

    expect(attachedAgentIds()).toEqual(["agent-1", "agent-2"]);
    await expect(bridge.sendTerminalInput("agent-1", "x")).rejects.toThrow(/not attached/);

    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    expect(attachedAgentIds()).toEqual(["agent-1", "agent-2", "agent-1"]);
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();
    await bridge.dispose();
  });
  /**
   * Scenario: hold a shown agent's attach unresolved, re-declare the same shown
   * set while it is still in flight — the shape a snapshot event takes once it
   * re-asserts the invariant — and only then let the attach land. Exactly one
   * attach may reach the daemon and exactly one scrollback may reach the pane:
   * `attachAgents` marks an agent attached before it invokes, so a pending
   * attach filters itself out of the next declaration. Without that, re-
   * asserting the invariant on every snapshot would open a second socket and
   * replay the same scrollback twice for one terminal.
   */
  it("does not attach twice when the shown set is re-declared while an attach is in flight", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const attach = holdableAttach();

    const bridge = new TauriDeckBridge();
    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    attach.hold("agent-1");
    const showing = bridge.setShownTerminals(["agent-1"]);
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1"]));

    const redeclaring = bridge.setShownTerminals(["agent-1"]);
    await settle();
    expect(attachedAgentIds()).toEqual(["agent-1"]);

    attach.release();
    await Promise.all([showing, redeclaring]);
    await settle();

    expect(attachedAgentIds()).toEqual(["agent-1"]);
    expect(detachCalls()).toHaveLength(0);
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();
    const replays = output.mock.calls.filter(([event]) => event.agentId === "agent-1" && event.operation === "replace");
    expect(replays).toHaveLength(1);
    await bridge.dispose();
  });

  /**
   * Scenario: hold one agent's `desktop_terminal_attach` unresolved, as a
   * stalled daemon would, then hide and re-show that terminal five times. Each
   * hide evicts it — eviction deletes `attached` and `pendingAttachments`,
   * because that marking IS how an in-flight attach is cancelled — so nothing
   * in the installed state stops the next show from starting a second command.
   * Exactly one `desktop_terminal_attach` may be outstanding all the same: the
   * Rust side serialises every agent through one attach gate with no timeout
   * and no cancellation, so one command per cycle grows a channel, a closure, a
   * promise and a queued command without bound, and becomes an attach/detach
   * storm if the daemon ever recovers (PRD #745 M7).
   */
  it("starts no second attach while one is still outstanding, however often the terminal is hidden and re-shown", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const attach = holdableAttach();

    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    attach.hold("agent-1");
    const showing = bridge.setShownTerminals(["agent-1"]);
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1"]));

    for (let cycle = 0; cycle < 5; cycle += 1) {
      await bridge.setShownTerminals([]);
      await bridge.setShownTerminals(["agent-1"]);
    }
    await settle();

    // One command, not six. And no detach either: the evictions found no
    // session to tear down, so nothing was queued behind the stall in either
    // direction.
    expect(attachedAgentIds()).toEqual(["agent-1"]);
    expect(detachCalls()).toHaveLength(0);

    attach.release();
    await Promise.allSettled([showing]);
    await bridge.dispose();
  });

  /**
   * Scenario: the counterpart to the test above — the same hide/reshow race,
   * but the daemon answers. The suppressed declaration must be coalesced rather
   * than dropped: when the outstanding attach settles it is cancelled (the pane
   * it belonged to is gone, so it detaches as an orphan), and exactly one
   * replacement attach follows for the terminal that IS on screen. Suppressing
   * without replaying would trade an unbounded queue for a dead pane, which is
   * the same defect the snapshot re-declaration above exists to fix.
   */
  it("attaches once more after the outstanding attach settles when the terminal was re-shown meanwhile", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const attach = holdableAttach();

    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();
    // Whatever attaches below is demand-driven: connect() left nothing behind.
    expect(attachedAgentIds()).toEqual([]);

    attach.hold("agent-1");
    const showing = bridge.setShownTerminals(["agent-1"]);
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1"]));

    // Away and straight back, faster than the attach round trip.
    await bridge.setShownTerminals([]);
    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    expect(attachedAgentIds()).toEqual(["agent-1"]);

    attach.release();
    // The queued declaration is replayed, so a SECOND attach names agent-1.
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1", "agent-1"]));
    attach.release();
    await Promise.allSettled([showing]);
    await settle();

    // The first session belonged to a pane that had gone; it is detached rather
    // than installed, and the pane on screen now takes input through the second.
    expect(detachedSessionIds()).toEqual(["session-agent-1-1"]);
    await expect(bridge.sendTerminalInput("agent-1", "x")).resolves.toBeUndefined();
    expect(invoke).toHaveBeenCalledWith("desktop_terminal_write", { sessionId: "session-agent-1-2", data: [120] });
    await bridge.dispose();
  });

  /**
   * Scenario: hold agent-1's `desktop_terminal_attach` unresolved and never
   * release it — the daemon accepted the attach and went silent. The guard
   * above then suppresses every later declaration for that agent, and the
   * replay that would lift the suppression is itself waiting on the invocation
   * that never settles, so the pane is dead for the life of the process.
   * Reconnect must genuinely re-arm it: `useDeckRuntime` memoizes the bridge on
   * `mode` and `reconnect()` calls `connect()` rather than disposing, so if
   * `connect()` does not clear the guard the user's only remedy silently does
   * nothing (PRD #745 M7).
   */
  it("re-arms an attach that never settled when the user reconnects", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const attach = holdableAttach();

    const bridge = new TauriDeckBridge();
    await bridge.subscribe(vi.fn(), vi.fn());
    await bridge.connect();

    attach.hold("agent-1");
    const showing = bridge.setShownTerminals(["agent-1"]);
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1"]));

    // Away and back while the daemon stays silent: suppressed, as designed.
    await bridge.setShownTerminals([]);
    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    expect(attachedAgentIds()).toEqual(["agent-1"]);

    // The first invocation stays outstanding for the whole test — it is the
    // thing being recovered from. Re-pointing `hold` at an agent that is never
    // shown only says the NEXT attach is answered, as a daemon that came back
    // would answer it.
    attach.hold("agent-2");

    // What the Reconnect button does — no dispose, the same bridge instance.
    await bridge.connect();
    // …and what the UI does next: the snapshot handler re-declares the shown
    // set. A second attach names agent-1, so the pane can come back.
    listeners.get("desktop://snapshot")?.({ payload: fleetSnapshot() });
    await vi.waitFor(() => expect(attachedAgentIds()).toEqual(["agent-1", "agent-1"]));

    // The stalled invocation finally answers; its session belongs to a channel
    // the recovered attach has replaced, so it is detached rather than
    // installed. Released here only so the pending promise settles.
    attach.release();
    await Promise.allSettled([showing]);
    await bridge.dispose();
  });

  /**
   * Scenario: drive the bridge with no terminal listener installed — no
   * `subscribe` — so every delivered chunk is buffered in `pendingTerminal`
   * instead. Show an agent, evict it by showing none, show it again, and only
   * then subscribe. The drain must replay the LIVE session's scrollback alone:
   * `evictTerminal` clears every map keyed by agent id, which its own doc
   * comment claims and `pendingTerminal` used to be the exception to, and a
   * surviving entry replays a torn-down pane ahead of the one on screen.
   */
  it("drops the buffered terminal chunks of an evicted agent", async () => {
    const { TauriDeckBridge } = await import("./bridge");
    const bridge = new TauriDeckBridge();
    await bridge.connect();

    await bridge.setShownTerminals(["agent-1"]);
    await settle();
    await bridge.setShownTerminals([]);
    await settle();
    await bridge.setShownTerminals(["agent-1"]);
    await settle();

    const output = vi.fn();
    await bridge.subscribe(vi.fn(), output);

    const replays = output.mock.calls
      .map(([event]) => event)
      .filter((event) => event.agentId === "agent-1" && event.operation === "replace");
    expect(replays).toHaveLength(1);
    // The live generation, not the evicted one: agent-1's first attach was
    // generation 1 and its replacement generation 2.
    expect(replays[0].generation).toBe(2);
    await bridge.dispose();
  });
});
