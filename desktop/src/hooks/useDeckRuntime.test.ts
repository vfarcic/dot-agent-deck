import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "../data/fixture";
import type { DeckBridge } from "../lib/bridge";
import { terminalInputState } from "../lib/terminalInput";
import type { TerminalChunk } from "../types";

/*
 * A bridge stub rather than `FixtureDeckBridge`, because the state under test
 * only exists when an action FAILS and the fixture bridge never rejects. Hoisted
 * so the `vi.mock` factory below — which vitest lifts above the imports — can
 * close over it.
 */
const { bridge } = vi.hoisted(() => ({
  bridge: {
    mode: "fixture",
    connect: vi.fn(),
    // Typed with the real two-callback shape rather than as a nullary stub, so
    // a test can capture the terminal channel the hook subscribes with.
    subscribe: vi.fn(async (_onFleet: (fleet: unknown) => void, _onTerminal: (event: TerminalChunk) => void) => () => {}),
    runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => {}),
    resizeTerminal: vi.fn(async () => {}),
    onTerminalGeometry: vi.fn(() => () => {}),
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

vi.mock("../lib/bridge", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/bridge")>()),
  selectRuntimeMode: () => "fixture" as const,
  createDeckBridge: () => bridge as unknown as DeckBridge,
}));

import { useDeckRuntime } from "./useDeckRuntime";

describe("useDeckRuntime", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // A FLEET, not one snapshot: PRD #742 M4 made `connect()` return every
    // observed deck, and `useDeckRuntime` ignores an empty one — so a bare
    // snapshot here leaves the hook on its loading seed and the test reads as
    // a connection failure rather than as the contract drift it is.
    bridge.connect.mockResolvedValue([createFixtureSnapshot("connected")]);
    bridge.subscribe.mockResolvedValue(() => {});
    bridge.onTerminalGeometry.mockReturnValue(() => {});
  });

  /**
   * Issue #1046: the runtime held the last action's error and exposed no way to
   * drop it, which is why the deck's toast had a dismiss button that could not
   * dismiss an error. Clearing must not disturb what the CONNECTION reports —
   * the banner reads `snapshot.connection`, and that is a different question
   * from whether the user has waved away the last failure.
   */
  it("clears the last action's error without touching the connection", async () => {
    bridge.runAction.mockRejectedValue(new Error("daemon returned error: publish-failed"));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      await expect(result.current.runAction({ type: "pause_run" })).rejects.toThrow("publish-failed");
    });
    expect(result.current.error).toBe("daemon returned error: publish-failed");

    act(() => result.current.clearError());

    expect(result.current.error).toBeUndefined();
    expect(result.current.snapshot.connection.status).toBe("connected");
  });

  /**
   * Scenario: submit text through the guarded verb and have the daemon answer
   * with a non-delivered verdict. The runtime records it against that agent so
   * the agent's terminal can say so — issue #1042, where `wrong-session` is
   * knowable ONLY by trying and is carried by no snapshot field.
   */
  it("records a non-delivered submit verdict against the agent", async () => {
    bridge.runAction.mockResolvedValue({ ok: false, sendResult: "wrong-session", message: "not delivered" });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "hello" });
    });

    expect(result.current.terminalInputResults).toEqual({ planner: "wrong-session" });
  });

  /**
   * Scenario: a later submit to the same agent is delivered. The recorded
   * verdict is dropped rather than left pinning a disabled input on a pane that
   * has just demonstrably accepted text.
   */
  it("drops a recorded verdict once a later submit is delivered", async () => {
    bridge.runAction.mockResolvedValue({ ok: false, sendResult: "wrong-session" });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));
    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "hello" });
    });
    expect(result.current.terminalInputResults).toEqual({ planner: "wrong-session" });

    bridge.runAction.mockResolvedValue({ ok: true, sendResult: "applied" });
    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "again" });
    });

    expect(result.current.terminalInputResults).toEqual({});
  });

  /**
   * Scenario: the PTY is respawned, which the attach stream reports as a
   * `replace` carrying a new generation. A verdict about the pane that existed
   * before the respawn no longer describes anything — and because the condition
   * is knowable only by SENDING, nothing else would ever clear it: the input it
   * disabled is the one that would have sent again.
   */
  it("clears a recorded verdict when the stream generation changes", async () => {
    let feedTerminal: ((event: TerminalChunk) => void) | undefined;
    bridge.subscribe.mockImplementation(async (_onFleet, onTerminal) => {
      feedTerminal = onTerminal;
      return () => {};
    });
    bridge.runAction.mockResolvedValue({ ok: false, sendResult: "wrong-session" });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));
    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "hello" });
    });
    const chunk = (generation: number, operation: TerminalChunk["operation"]): TerminalChunk => ({
      agentId: "planner",
      data: new TextEncoder().encode("output"),
      stream: "output",
      operation,
      generation,
    });

    // Ordinary output on the generation the verdict was recorded against.
    act(() => feedTerminal?.(chunk(1, "append")));
    expect(result.current.terminalInputResults).toEqual({ planner: "wrong-session" });

    // The respawn: a fresh attach replays its scrollback as a `replace`.
    act(() => feedTerminal?.(chunk(2, "replace")));

    expect(result.current.terminalInputResults).toEqual({});
  });

  /**
   * Scenario: a verdict is recorded against an agent, and the daemon then
   * pushes a fresh snapshot in which that agent's pane reads a writable lease.
   * The record is one PAST attempt and the snapshot is newer state, so the
   * record is dropped and the terminal goes back to accepting input.
   *
   * Without this rule the recorded `wrong-session` outranks a writable lease in
   * `terminalInputState`, and nothing the user can do clears it: their typing
   * goes through `sendTerminalInput` (the raw stream), never `submit_text`, and
   * a write lease can return to this client with no PTY respawn to trip the
   * generation route above.
   */
  it("drops a recorded verdict when a fresh snapshot reports a writable lease", async () => {
    let pushFleet: ((fleet: unknown) => void) | undefined;
    bridge.subscribe.mockImplementation(async (onFleet, _onTerminal) => {
      pushFleet = onFleet;
      return () => {};
    });
    bridge.runAction.mockResolvedValue({ ok: false, sendResult: "wrong-session" });
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));
    await act(async () => {
      await result.current.runAction({ type: "submit_text", agentId: "planner", text: "hello" });
    });
    expect(result.current.terminalInputResults).toEqual({ planner: "wrong-session" });

    // The lease is back with this client and the pane is live — which is
    // exactly the state a rollover leaves behind, so the snapshot says
    // "writable" while the verdict says "wrong-session".
    const fresh = createFixtureSnapshot("connected");
    const writable = {
      ...fresh,
      agents: fresh.agents.map((agent) => (agent.id === "planner" ? { ...agent, status: "running" as const, writeLease: "write" as const } : agent)),
    };
    act(() => pushFleet?.([writable]));

    expect(result.current.terminalInputResults).toEqual({});
    // And the consequence the user sees: the input the snapshot says is
    // writable is no longer held disabled by the record.
    const planner = result.current.snapshot.agents.find((agent) => agent.id === "planner")!;
    expect(planner.writeLease).toBe("write");
    expect(terminalInputState(planner, result.current.terminalInputResults?.[planner.id]).readOnly).toBe(false);
  });
});
