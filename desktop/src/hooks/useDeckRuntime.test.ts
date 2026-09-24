import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, FIXTURE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID } from "../data/fixture";
import { agentKey } from "../lib/agentKey";
import type { DeckBridge } from "../lib/bridge";
import { LaunchCleanupError } from "../lib/actionError";
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
    declareVoiceScreen: vi.fn(),
    dispose: vi.fn(async () => {}),
  },
}));

vi.mock("../lib/bridge", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/bridge")>()),
  selectRuntimeMode: () => "fixture" as const,
  createDeckBridge: () => bridge as unknown as DeckBridge,
}));

import { useDeckRuntime } from "./useDeckRuntime";

/**
 * How the runtime's per-agent maps are addressed since PRD #1105's security
 * audit: the COMPOSITE `(deckId, agentId)`, because agent ids are per-daemon
 * monotonic and collide across decks. The fixture's deck is the one every
 * record below is made against.
 */
const key = (agentId: string) => agentKey(FIXTURE_DAEMON_ID, agentId);

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
   * Scenario (PRD #1223): a voice declaration made through the runtime carries
   * the New agent dialog's deck step for the fleet the runtime holds — the
   * same list the dialog preselects from — so an unreachable deck reaches Rust
   * with the reason the step shows, and the panel had to say nothing about it.
   */
  it("declares the fleet's deck step with every voice declaration", async () => {
    bridge.connect.mockResolvedValue(createFixtureFleet("fleet"));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));

    act(() => result.current.declareVoiceScreen?.("overview"));

    expect(bridge.declareVoiceScreen).toHaveBeenCalledTimes(1);
    const [screen, directories, newAgent, deckStep] = bridge.declareVoiceScreen.mock.calls[0];
    expect([screen, directories, newAgent]).toEqual(["overview", undefined, undefined]);
    expect(deckStep).toContainEqual({ deckId: FIXTURE_DAEMON_ID });
    expect(deckStep).toContainEqual({ deckId: FIXTURE_UNREACHABLE_DAEMON_ID, reason: "No deck is listening on the configured socket." });
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

    expect(result.current.terminalInputResults).toEqual({ [key("planner")]: "wrong-session" });
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
    expect(result.current.terminalInputResults).toEqual({ [key("planner")]: "wrong-session" });

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
    expect(result.current.terminalInputResults).toEqual({ [key("planner")]: "wrong-session" });

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
    expect(result.current.terminalInputResults).toEqual({ [key("planner")]: "wrong-session" });

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
    expect(terminalInputState(planner, result.current.terminalInputResults?.[key(planner.id)]).readOnly).toBe(false);
  });
  /**
   * Scenario: two actions are in flight at once. The first rejects with a
   * `LaunchCleanupError` naming roles that may still be running; the second
   * then rejects ordinarily. The runtime must report the second failure's
   * sentence with NO cleanup roles beside it — PRD #1223 audit follow-up W1.
   *
   * This is the sequencing seam, not the rendering: while the message and the
   * roles were two `useState` calls, the later ordinary rejection replaced the
   * message alone and left the earlier launch's roles attached to it, so the
   * toast named roles the failure on screen had never touched.
   */
  it("never leaves an earlier failure's cleanup roles beside a later failure's message", async () => {
    let rejectFirst: ((cause: unknown) => void) | undefined;
    let rejectSecond: ((cause: unknown) => void) | undefined;
    bridge.runAction
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectFirst = reject; }))
      .mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectSecond = reject; }));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      // Both start before either answers, so the second one's clear runs first
      // and neither rejection can be read as "the only one in flight".
      const first = result.current.runAction({ type: "pause_run" }).catch(() => {});
      const second = result.current.runAction({ type: "pause_run" }).catch(() => {});
      rejectFirst?.(new LaunchCleanupError("launch failed; cleanup could not confirm stop for 2 role(s)", ["planner", "coder"]));
      rejectSecond?.(new Error("daemon returned error: publish-failed"));
      await Promise.all([first, second]);
    });

    expect(result.current.error).toBe("daemon returned error: publish-failed");
    expect(result.current.errorCleanup).toBeUndefined();
  });

  /**
   * The same seam with `reconnect()` interleaved (PRD #1223 audit follow-up
   * W1): a launch is in flight, Refresh starts and clears the failure, the
   * launch then rejects with its roles, and the reconnect fails afterwards.
   * Its failure path writes a sentence and nothing else, so the launch's roles
   * must not survive into it either.
   */
  it("never leaves a launch's cleanup roles beside a failed reconnect", async () => {
    let rejectLaunch: ((cause: unknown) => void) | undefined;
    let rejectConnect: ((cause: unknown) => void) | undefined;
    bridge.runAction.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectLaunch = reject; }));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      const launch = result.current.runAction({ type: "pause_run" }).catch(() => {});
      // Refresh, which clears the failure before either answer lands.
      bridge.connect.mockImplementationOnce(() => new Promise((_resolve, reject) => { rejectConnect = reject; }));
      const refresh = result.current.reconnect();
      rejectLaunch?.(new LaunchCleanupError("launch failed; cleanup could not confirm stop for 1 role(s)", ["planner"]));
      rejectConnect?.(new Error("the deck is not answering"));
      await Promise.all([launch, refresh]);
    });

    expect(result.current.error).toBe("the deck is not answering");
    expect(result.current.errorCleanup).toBeUndefined();
  });
});
