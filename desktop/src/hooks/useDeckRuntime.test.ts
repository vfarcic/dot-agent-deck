import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureSnapshot, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID, FIXTURE_UNREACHABLE_DAEMON_ID, restartedFixtureSnapshot } from "../data/fixture";
import { agentKey, boundAgentKey } from "../lib/agentKey";
import { deckName } from "../lib/displayText";
import type { DeckBridge } from "../lib/bridge";
import { LaunchCleanupError } from "../lib/actionError";
import type { AgentCardsSnapshot, DeckFleet, TerminalChunk } from "../types";

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
    reconnectDeck: vi.fn(),
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
   * Issue #1994: a deck's own Reconnect names its deck. Only that deck is
   * marked "Reconnecting…", the bridge is asked to reconnect it by id and not
   * to re-establish the fleet, and the selected deck — the one the unnamed
   * reconnect would have marked — is never touched.
   */
  /** Scenario: Reconnects the named deck and leaves the selected deck alone. */
  it("reconnects the named deck and leaves the selected deck alone", async () => {
    const fleet = createFixtureFleet("fleet");
    bridge.connect.mockResolvedValue(fleet);
    let answer: ((fleet: DeckFleet) => void) | undefined;
    bridge.reconnectDeck.mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));
    const statusOf = (deckId: string) => result.current.fleet.find((deck) => deck.connection.deckId === deckId)?.connection;
    expect(result.current.fleet[0].connection.deckId).toBe(FIXTURE_DAEMON_ID);
    const before = statusOf(FIXTURE_DAEMON_ID);

    let pending: Promise<void> | undefined;
    act(() => { pending = result.current.reconnect(FIXTURE_UNREACHABLE_DAEMON_ID); });

    expect(statusOf(FIXTURE_UNREACHABLE_DAEMON_ID)).toMatchObject({ status: "loading", message: "Reconnecting…" });
    expect(statusOf(FIXTURE_DAEMON_ID)).toBe(before);
    expect(bridge.reconnectDeck).toHaveBeenCalledWith(FIXTURE_UNREACHABLE_DAEMON_ID);
    expect(bridge.connect).toHaveBeenCalledTimes(1);

    await act(async () => {
      answer?.(fleet);
      await pending;
    });
    expect(statusOf(FIXTURE_DAEMON_ID)?.status).toBe("connected");
  });

  /** Scenario: A named deck's failed reconnect marks that deck failed, keeps naming it, and leaves the selected deck connected. */
  it("puts a named deck's failed reconnect on that deck alone", async () => {
    bridge.connect.mockResolvedValue(createFixtureFleet("fleet"));
    bridge.reconnectDeck.mockRejectedValueOnce(new Error("the host is not answering"));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));
    const deckOf = (deckId: string) => result.current.fleet.find((deck) => deck.connection.deckId === deckId);

    await act(async () => {
      await result.current.reconnect(FIXTURE_UNREACHABLE_DAEMON_ID);
    });

    expect(result.current.error).toBe("the host is not answering");
    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.connection).toMatchObject({ status: "error", message: "the host is not answering", deckId: FIXTURE_UNREACHABLE_DAEMON_ID });
    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.health).toBe("failed");
    expect(result.current.fleet[0].connection).toMatchObject({ status: "connected", deckId: FIXTURE_DAEMON_ID });
  });

  /** Scenario: Reconnect is pressed on two decks at once; the first succeeds and its fleet replaces the second's "Reconnecting…", then the second fails: the second deck shows its failure (PR #2322 review). */
  it("puts a deck's failure on it even after another deck's reconnect answered first", async () => {
    const fleet = createFixtureFleet("fleet");
    bridge.connect.mockResolvedValue(fleet);
    let answerRemote: ((fleet: DeckFleet) => void) | undefined;
    let rejectUnreachable: ((cause: unknown) => void) | undefined;
    bridge.reconnectDeck.mockImplementation((deckId: string) => new Promise((resolve, reject) => {
      if (deckId === FIXTURE_REMOTE_DAEMON_ID) answerRemote = resolve;
      else rejectUnreachable = reject;
    }));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(2));
    const deckOf = (deckId: string) => result.current.fleet.find((deck) => deck.connection.deckId === deckId);

    let remote: Promise<void> | undefined;
    let unreachable: Promise<void> | undefined;
    act(() => { remote = result.current.reconnect(FIXTURE_REMOTE_DAEMON_ID); });
    act(() => { unreachable = result.current.reconnect(FIXTURE_UNREACHABLE_DAEMON_ID); });
    await act(async () => {
      answerRemote?.(fleet);
      await remote;
    });
    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.connection.status).not.toBe("loading");
    await act(async () => {
      rejectUnreachable?.(new Error("the host is not answering"));
      await unreachable;
    });

    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.connection).toMatchObject({ status: "error", message: "the host is not answering" });
    expect(result.current.error).toBe("the host is not answering");
  });

  /** Scenario: Two Reconnects on one deck, the first failing after the second started: only the second decides what the deck shows (PR #2322 review). */
  it("applies only the latest reconnect of a deck", async () => {
    const fleet = createFixtureFleet("fleet");
    bridge.connect.mockResolvedValue(fleet);
    let rejectFirst: ((cause: unknown) => void) | undefined;
    let answerSecond: ((fleet: DeckFleet) => void) | undefined;
    bridge.reconnectDeck
      .mockImplementationOnce(() => new Promise((_resolve, fail) => { rejectFirst = fail; }))
      .mockImplementationOnce(() => new Promise((resolve) => { answerSecond = resolve; }));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));
    const deckOf = (deckId: string) => result.current.fleet.find((deck) => deck.connection.deckId === deckId);

    let first: Promise<void> | undefined;
    let second: Promise<void> | undefined;
    act(() => { first = result.current.reconnect(FIXTURE_UNREACHABLE_DAEMON_ID); });
    act(() => { second = result.current.reconnect(FIXTURE_UNREACHABLE_DAEMON_ID); });
    await act(async () => {
      rejectFirst?.(new Error("the first attempt timed out"));
      await first;
    });
    expect(result.current.error).toBeUndefined();
    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.connection.status).toBe("loading");

    const recovered = fleet.map((deck) => deck.connection.deckId === FIXTURE_UNREACHABLE_DAEMON_ID ? { ...deck, connection: { ...deck.connection, status: "connected" as const } } : deck);
    await act(async () => {
      answerSecond?.(recovered);
      await second;
    });
    expect(deckOf(FIXTURE_UNREACHABLE_DAEMON_ID)?.connection.status).toBe("connected");
  });

  /** Scenario: Reconnect with no deck named still re-establishes the whole fleet, as the dashboard's Refresh does. */
  it("re-establishes the whole fleet when no deck is named", async () => {
    bridge.connect.mockResolvedValue(createFixtureFleet("fleet"));
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.fleet.length).toBeGreaterThan(1));

    await act(async () => {
      await result.current.reconnect();
    });

    expect(bridge.connect).toHaveBeenCalledTimes(2);
    expect(bridge.reconnectDeck).not.toHaveBeenCalled();
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

/*
 * PRD #2374 review B6 — a deck's agent cards are replaced only by a snapshot
 * from the daemon process its connection names now, never by an older
 * revision of that process's cards, whichever way the snapshot arrives (a
 * push or a direct read) and in whatever order; and a daemon replaced while
 * its deck stays connected takes its cards with it.
 */
describe("useDeckRuntime agent cards (PRD #2374)", () => {
  type CardsBridge = {
    consumeAgentCards?: () => Promise<void>;
    subscribeAgentCards?: (listener: (snapshot: AgentCardsSnapshot) => void) => Promise<() => void>;
    agentCards?: (deckId: string) => Promise<AgentCardsSnapshot | null>;
  };
  const cardsBridge = bridge as unknown as CardsBridge;
  const cards = (instanceId: string, revision: number): AgentCardsSnapshot => ({ deckId: FIXTURE_DAEMON_ID, instanceId, revision, entries: [], catalogErrors: [] });

  /* The order the card calls reach the bridge in. */
  let calls: string[] = [];
  /* The runtime, with cards consumed the way the Cards switch does on mount. */
  const renderConsuming = () => {
    const rendered = renderHook(() => useDeckRuntime());
    act(() => rendered.result.current.consumeAgentCards?.());
    return rendered;
  };

  beforeEach(() => {
    vi.clearAllMocks();
    bridge.onTerminalGeometry.mockReturnValue(() => {});
    calls = [];
    // What keeps cards unread is that nothing consumed them; the
    // no-cards case is the component's: desktop/agent-cards/007.
    cardsBridge.consumeAgentCards = vi.fn(async () => { calls.push("consume"); });
  });
  afterEach(() => {
    delete cardsBridge.consumeAgentCards;
    delete cardsBridge.subscribeAgentCards;
    delete cardsBridge.agentCards;
  });

  /*
   * PR #2378 review: the backend consumes cards only on the webview's
   * request, and the webview requests them only from the surface that renders
   * them. Until that surface calls `consumeAgentCards` the hook asks for,
   * subscribes to and reads nothing; once called, the
   * request comes first and exactly once, then the listener and the read.
   */
  it("subscribes to and reads no cards until a surface consumes them", async () => {
    cardsBridge.subscribeAgentCards = vi.fn(async () => () => {});
    cardsBridge.agentCards = vi.fn(async () => null);
    bridge.connect.mockResolvedValue([createFixtureSnapshot("docs-restart")]);
    const { result } = renderHook(() => useDeckRuntime());
    await waitFor(() => expect(result.current.snapshot.connection.incarnation).toBe("docs-before"));
    await act(async () => { await Promise.resolve(); });
    expect(result.current.consumeAgentCards).toBeTypeOf("function");
    expect(cardsBridge.consumeAgentCards).not.toHaveBeenCalled();
    expect(cardsBridge.subscribeAgentCards).not.toHaveBeenCalled();
    expect(cardsBridge.agentCards).not.toHaveBeenCalled();
  });

  it("asks the backend for cards once, before listening and reading, however often it is consumed", async () => {
    cardsBridge.subscribeAgentCards = vi.fn(async () => { calls.push("subscribe"); return () => {}; });
    cardsBridge.agentCards = vi.fn(async () => { calls.push("read"); return null; });
    bridge.connect.mockResolvedValue([createFixtureSnapshot("docs-restart")]);
    const { result } = renderConsuming();
    // A second surface mounting, or the same one mounting again.
    act(() => result.current.consumeAgentCards?.());
    await waitFor(() => expect(calls).toContain("subscribe"));
    await waitFor(() => expect(calls).toContain("read"));
    act(() => result.current.consumeAgentCards?.());
    expect(calls[0]).toBe("consume");
    expect(calls.filter((call) => call === "consume")).toHaveLength(1);
    expect(cardsBridge.subscribeAgentCards).toHaveBeenCalledTimes(1);
  });

  it("accepts cards only from the connected process, ignores late and older ones, and drops a replaced daemon's", async () => {
    let onFleet: ((fleet: DeckFleet) => void) | undefined;
    bridge.subscribe.mockImplementation(async (fleetListener) => {
      onFleet = fleetListener as (fleet: DeckFleet) => void;
      return () => {};
    });
    let push: ((snapshot: AgentCardsSnapshot) => void) | undefined;
    const reads: ((snapshot: AgentCardsSnapshot | null) => void)[] = [];
    cardsBridge.subscribeAgentCards = vi.fn(async (listener) => {
      push = listener;
      return () => {};
    });
    cardsBridge.agentCards = vi.fn(() => new Promise<AgentCardsSnapshot | null>((resolve) => { reads.push(resolve); }));
    bridge.connect.mockResolvedValue([createFixtureSnapshot("docs-restart")]);
    const { result } = renderConsuming();
    await waitFor(() => expect(result.current.snapshot.connection.incarnation).toBe("docs-before"));
    await waitFor(() => expect(reads).toHaveLength(1));
    await waitFor(() => expect(push).toBeDefined());
    const held = () => result.current.agentCards?.[FIXTURE_DAEMON_ID];

    act(() => push!(cards("docs-before", 5)));
    expect(held()).toMatchObject({ instanceId: "docs-before", revision: 5 });
    // The read asked on connect lands after the push, older: ignored.
    await act(async () => { reads[0](cards("docs-before", 3)); });
    expect(held()?.revision).toBe(5);
    // A push delayed behind a newer one: ignored.
    act(() => push!(cards("docs-before", 4)));
    expect(held()?.revision).toBe(5);
    // Another process's cards while the connection still names this one: not shown.
    act(() => push!(cards("docs-after", 1)));
    expect(held()).toMatchObject({ instanceId: "docs-before", revision: 5 });

    // The daemon is replaced and the deck stays connected: the old cards go,
    // the successor's push (held until the fleet named it) is shown, and its
    // cards are read again.
    act(() => onFleet!([restartedFixtureSnapshot()]));
    expect(held()).toMatchObject({ instanceId: "docs-after", revision: 1 });
    await waitFor(() => expect(reads).toHaveLength(2));
    // A late answer from the replaced process changes nothing.
    await act(async () => { reads[1](cards("docs-before", 99)); });
    expect(held()).toMatchObject({ instanceId: "docs-after", revision: 1 });
  });

  /*
   * PR #2491 audit (A2) — the Card actions editor names the daemon process
   * each read and edit is bound to, the one whose list the user saw; the
   * runtime hands the bridge exactly that, never the process whose cards
   * happen to be shown when Save is pressed, and passes a missing one on for
   * the bridge to refuse (A1) rather than filling it in.
   */
  it("passes the Card actions editor's daemon process to the bridge as given", async () => {
    type CatalogBridge = { readCardCatalog?: (deckId: string, instanceId: string | undefined) => Promise<unknown>; editCardCatalog?: (deckId: string, edit: unknown, instanceId: string | undefined) => Promise<unknown> };
    const catalogBridge = bridge as unknown as CatalogBridge;
    const catalog = { actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" };
    catalogBridge.readCardCatalog = vi.fn(async () => catalog);
    catalogBridge.editCardCatalog = vi.fn(async () => catalog);
    try {
      let onFleet: ((fleet: DeckFleet) => void) | undefined;
      bridge.subscribe.mockImplementation(async (fleetListener) => {
        onFleet = fleetListener as (fleet: DeckFleet) => void;
        return () => {};
      });
      let push: ((snapshot: AgentCardsSnapshot) => void) | undefined;
      cardsBridge.subscribeAgentCards = vi.fn(async (listener) => {
        push = listener;
        return () => {};
      });
      cardsBridge.agentCards = vi.fn(async () => null);
      bridge.connect.mockResolvedValue([createFixtureSnapshot("docs-restart")]);
      const { result } = renderConsuming();
      await waitFor(() => expect(result.current.snapshot.connection.incarnation).toBe("docs-before"));
      await waitFor(() => expect(push).toBeDefined());
      act(() => push!(cards("docs-before", 1)));
      const removal = { kind: "remove" as const, name: "ship" };
      await result.current.readCardCatalog!(FIXTURE_DAEMON_ID, "docs-before");
      expect(catalogBridge.readCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "docs-before");

      /* The deck's daemon restarts: an edit made from the old process's list still names the old process. */
      act(() => push!(cards("docs-after", 1)));
      act(() => onFleet!([restartedFixtureSnapshot()]));
      expect(result.current.agentCards?.[FIXTURE_DAEMON_ID]).toMatchObject({ instanceId: "docs-after" });
      await result.current.editCardCatalog!(FIXTURE_DAEMON_ID, removal, "docs-before");
      expect(catalogBridge.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, removal, "docs-before");
      await result.current.readCardCatalog!(FIXTURE_DAEMON_ID, undefined);
      expect(catalogBridge.readCardCatalog, "a missing process is not filled in from the cards shown").toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, undefined);
      await result.current.editCardCatalog!(FIXTURE_DAEMON_ID, removal, undefined);
      expect(catalogBridge.editCardCatalog).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, removal, undefined);
    } finally {
      delete catalogBridge.readCardCatalog;
      delete catalogBridge.editCardCatalog;
    }
  });

  it("drops a deck's cards when the daemon is replaced and shows nothing until the successor's arrive", async () => {
    let onFleet: ((fleet: DeckFleet) => void) | undefined;
    bridge.subscribe.mockImplementation(async (fleetListener) => {
      onFleet = fleetListener as (fleet: DeckFleet) => void;
      return () => {};
    });
    let push: ((snapshot: AgentCardsSnapshot) => void) | undefined;
    cardsBridge.subscribeAgentCards = vi.fn(async (listener) => {
      push = listener;
      return () => {};
    });
    bridge.connect.mockResolvedValue([createFixtureSnapshot("docs-restart")]);
    const { result } = renderConsuming();
    await waitFor(() => expect(push).toBeDefined());
    await waitFor(() => expect(result.current.snapshot.connection.incarnation).toBe("docs-before"));
    act(() => push!(cards("docs-before", 7)));
    expect(result.current.agentCards?.[FIXTURE_DAEMON_ID]?.revision).toBe(7);
    act(() => onFleet!([restartedFixtureSnapshot()]));
    expect(result.current.agentCards?.[FIXTURE_DAEMON_ID]).toBeUndefined();
  });
});
