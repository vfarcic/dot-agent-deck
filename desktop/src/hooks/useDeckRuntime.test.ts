import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID, restartedFixtureSnapshot } from "../data/fixture";
import { agentKey, boundAgentKey } from "../lib/agentKey";
import { deckName } from "../lib/displayText";
import type { DeckBridge } from "../lib/bridge";
import { LaunchCleanupError } from "../lib/actionError";
import type { DeckFleet, TerminalChunk } from "../types";

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
    onTerminalGeometry: vi.fn((_listener: (agentId: string, rows: number, cols: number, deckId?: string, incarnation?: string) => void) => () => {}),
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
   * drop it, which is why the daemon's toast had a dismiss button that could not
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
   * same list the dialog preselects from — so an unreachable deck, which the
   * dialog does not list, reaches Rust with its short reason class (PR #1451
   * round 3), and the panel had to say nothing about it.
   */
  /** Scenario: Declares the fleet's deck step with every voice declaration. */
  it("declares the fleet's deck step with every voice declaration", async () => {
    bridge.connect.mockResolvedValue(createFixtureFleet("fleet"));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));

    act(() => result.current.declareVoiceScreen?.("overview"));

    expect(bridge.declareVoiceScreen).toHaveBeenCalledTimes(1);
    const [screen, directories, newAgent, deckStep] = bridge.declareVoiceScreen.mock.calls[0];
    expect([screen, directories, newAgent]).toEqual(["overview", undefined, undefined]);
    expect(deckStep).toContainEqual({ deckId: FIXTURE_DAEMON_ID });
    expect(deckStep).toContainEqual({ deckId: FIXTURE_UNREACHABLE_DAEMON_ID, reason: "it is not connected" });
  });

  /**
   * Scenario: two actions are in flight at once. The first rejects with a
   * `LaunchCleanupError` naming roles that may still be running; the second
   * then rejects ordinarily, in the same batch. The runtime must report the
   * second failure's sentence AND still hold the first one's roles, as a
   * warning of their own — issue #1234.
   *
   * PRD #1223 audit W1 fixed half of this seam: the roles must never read as
   * belonging to the later sentence. The other half is that they must not be
   * lost either — while they lived in the single failure slot, the ordinary
   * rejection replaced the whole value and no frame ever named them.
   */
  it("keeps an earlier failure's cleanup roles as their own warning when a later failure replaces its message", async () => {
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
    expect(result.current.cleanupWarnings?.map((warning) => warning.stops)).toEqual([["planner", "coder"]]);
  });

  /**
   * The same seam with `reconnect()` interleaved (PRD #1223 audit follow-up
   * W1, issue #1234): a launch is in flight, Refresh starts and clears the
   * failure, the launch then rejects with its roles, and the reconnect fails
   * afterwards. The reconnect's sentence replaces the error; the launch's
   * roles stay queued as their own warning.
   */
  /** Scenario: Keeps a launch's cleanup roles through a failed reconnect. */
  it("keeps a launch's cleanup roles through a failed reconnect", async () => {
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
      rejectConnect?.(new Error("the daemon is not answering"));
      await Promise.all([launch, refresh]);
    });

    expect(result.current.error).toBe("the daemon is not answering");
    expect(result.current.cleanupWarnings?.map((warning) => warning.stops)).toEqual([["planner"]]);
  });

  /**
   * Issue #1234: every other writer of the failure slot clears it — a new
   * action, `reconnect()`, and the toast's own dismissal through
   * `clearError`. None of them may take a cleanup warning with it; only
   * dismissing THAT warning does, and a dismissal names one warning, so a
   * second one naming the very same roles survives it.
   */
  it("ends a cleanup warning only when that warning is dismissed", async () => {
    bridge.runAction.mockRejectedValue(new LaunchCleanupError("launch failed; cleanup could not confirm stop for 1 role(s)", ["planner"]));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.status).toBe("connected"));

    await act(async () => {
      await result.current.runAction({ type: "pause_run" }).catch(() => {});
    });
    expect(result.current.cleanupWarnings).toHaveLength(1);

    bridge.runAction.mockResolvedValue({ ok: true });
    await act(async () => {
      await result.current.runAction({ type: "pause_run" });
      await result.current.reconnect();
    });
    act(() => result.current.clearError());
    expect(result.current.error).toBeUndefined();
    expect(result.current.cleanupWarnings?.map((warning) => warning.stops)).toEqual([["planner"]]);

    // A second rejection naming the same role is a second warning.
    bridge.runAction.mockRejectedValue(new LaunchCleanupError("launch failed; cleanup could not confirm stop for 1 role(s)", ["planner"]));
    await act(async () => {
      await result.current.runAction({ type: "pause_run" }).catch(() => {});
    });
    const [first, second] = result.current.cleanupWarnings ?? [];
    expect(second).toBeDefined();
    expect(first.id).not.toBe(second.id);

    act(() => result.current.dismissCleanupWarning?.(first.id));
    expect(result.current.cleanupWarnings).toEqual([second]);
    act(() => result.current.dismissCleanupWarning?.(second.id));
    expect(result.current.cleanupWarnings).toEqual([]);
  });

  /**
   * Issue #1234 review: a warning outlives the selection it was raised under,
   * so it must name the deck its action was SENT to. A deck-scoped close of an
   * orchestration on the remote deck, from a fleet whose selected deck is the
   * local one, is labelled with the remote deck; an action that names no deck
   * is labelled with the deck that was selected when it was sent.
   */
  it("labels a cleanup warning with the deck its action was sent to", async () => {
    bridge.connect.mockResolvedValue(createFixtureFleet("fleet"));
    bridge.runAction.mockRejectedValue(new LaunchCleanupError("close failed; cleanup could not confirm stop for 1 role(s)", ["planner"]));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));
    const remote = result.current.fleet.find((deck) => deck.connection.deckId === FIXTURE_REMOTE_DAEMON_ID)!;

    await act(async () => {
      await result.current.runAction({ type: "stop_orchestration", deckId: FIXTURE_REMOTE_DAEMON_ID, roles: [{ agentId: "agent-1", name: "planner" }] }).catch(() => {});
      await result.current.runAction({ type: "pause_run" }).catch(() => {});
    });

    expect(result.current.snapshot.connection.deckId).not.toBe(FIXTURE_REMOTE_DAEMON_ID);
    expect(result.current.cleanupWarnings?.map((warning) => warning.deck)).toEqual([deckName(remote.connection), "Local daemon"]);
  });

  /**
   * Scenario: a deck lists two agents, both stream output and report a grid,
   * and an agent on another deck streams too. Then the deck lists one of them
   * (the other was closed), drops its link, and comes back as a restarted
   * process listing nobody. Each closed or restarted-away agent's retained
   * output and grid are gone once its deck's listing stops naming it; a
   * disconnected deck and another deck keep theirs (issue #1845, finding 1).
   */
  it("forgets a closed or restarted agent's terminal buffer and grid once its deck's listing stops naming it", async () => {
    let onFleet: ((fleet: DeckFleet) => void) | undefined;
    let onTerminal: ((event: TerminalChunk) => void) | undefined;
    let onGeometry: ((agentId: string, rows: number, cols: number, deckId?: string, incarnation?: string) => void) | undefined;
    bridge.subscribe.mockImplementation(async (fleetListener, terminalListener) => {
      onFleet = fleetListener as (fleet: DeckFleet) => void;
      onTerminal = terminalListener;
      return () => {};
    });
    bridge.onTerminalGeometry.mockImplementation((listener) => {
      onGeometry = listener;
      return () => {};
    });
    const before = createFixtureSnapshot("docs-restart");
    bridge.connect.mockResolvedValue([before]);
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.incarnation).toBe("docs-before"));

    const OTHER_DECK = "/tmp/other-deck.sock";
    const output = (deckId: string, agentId: string, incarnation: string) =>
      onTerminal!({ deckId, agentId, incarnation, data: new TextEncoder().encode(`out of ${agentId}`), stream: "output", operation: "append" });
    act(() => {
      output(FIXTURE_DAEMON_ID, "1", "docs-before");
      output(FIXTURE_DAEMON_ID, "2", "docs-before");
      output(OTHER_DECK, "1", "elsewhere");
      onGeometry!("1", 24, 80, FIXTURE_DAEMON_ID, "docs-before");
      onGeometry!("2", 24, 80, FIXTURE_DAEMON_ID, "docs-before");
    });
    const buffer = (deckId: string, agentId: string, incarnation: string) => result.current.terminalFeed?.get(deckId, agentId, incarnation);
    const grid = (agentId: string) => result.current.appliedGeometry?.[boundAgentKey({ deckId: FIXTURE_DAEMON_ID, agentId, incarnation: "docs-before" })];
    expect(buffer(FIXTURE_DAEMON_ID, "1", "docs-before")).toBeDefined();
    expect(buffer(FIXTURE_DAEMON_ID, "2", "docs-before")).toBeDefined();
    expect(grid("2")).toEqual({ rows: 24, cols: 80 });

    // Agent 2 closed: the same process lists agent 1 alone.
    act(() => onFleet!([{ ...before, agents: before.agents.filter((agent) => agent.id === "1") }]));
    expect(buffer(FIXTURE_DAEMON_ID, "2", "docs-before")).toBeUndefined();
    expect(grid("2")).toBeUndefined();
    expect(buffer(FIXTURE_DAEMON_ID, "1", "docs-before")).toBeDefined();
    expect(grid("1")).toEqual({ rows: 24, cols: 80 });

    // A deck that is not answering lists nobody and vouches for nothing: keep.
    act(() => onFleet!([{ ...before, connection: { status: "disconnected", deckId: FIXTURE_DAEMON_ID, socketPath: FIXTURE_DAEMON_ID, message: "Daemon not responding" }, agents: [] }]));
    expect(buffer(FIXTURE_DAEMON_ID, "1", "docs-before")).toBeDefined();
    expect(grid("1")).toEqual({ rows: 24, cols: 80 });

    // The deck restarted: another process, listing nobody.
    act(() => onFleet!([restartedFixtureSnapshot()]));
    expect(buffer(FIXTURE_DAEMON_ID, "1", "docs-before")).toBeUndefined();
    expect(grid("1")).toBeUndefined();
    // Another deck's agent is not this deck's to forget.
    expect(buffer(OTHER_DECK, "1", "elsewhere")).toBeDefined();
  });
});
