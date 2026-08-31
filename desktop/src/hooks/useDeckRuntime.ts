import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createFixtureSnapshot } from "../data/fixture";
import { createDeckBridge, selectRuntimeMode } from "../lib/bridge";
import { applyTerminalChunk } from "../lib/terminalBuffer";
const EMPTY_TERMINAL_DATA: Record<string, TerminalBuffer> = {};
import type { DeckAction, DeckRuntimeState, DeckSnapshot, TerminalBuffer } from "../types";

export function useDeckRuntime(): DeckRuntimeState {
  const mode = useMemo(selectRuntimeMode, []);
  const bridge = useMemo(() => createDeckBridge(mode), [mode]);
  const [snapshot, setSnapshot] = useState<DeckSnapshot>(() => {
    if (mode === "live") {
      // PR #416 review B2: live mode gets its own HONEST empty seed. The old
      // seed spread createFixtureSnapshot("empty"), which empties the arrays
      // but keeps the scalars — so with no daemon running the top bar showed a
      // fixture branch, run id, elapsed time and node count as steady state,
      // with no DEMO DATA banner to disclaim them. Nothing here is invented:
      // every field says "unavailable" until the daemon says otherwise.
      const fixtureShape = createFixtureSnapshot("empty");
      return {
        ...fixtureShape,
        runId: "—",
        repo: "No active project",
        branch: "Unavailable",
        worktree: "No active project",
        elapsed: "—",
        spend: 0,
        currentNode: 0,
        totalNodes: 0,
        currentAttempt: 0,
        stages: [],
        agents: [],
        evidence: [],
        handoffs: [],
        connection: { status: "loading", message: "Connecting to local daemon…" },
      };
    }
    const initial = createFixtureSnapshot("empty");
    return {
      ...initial,
      connection: { status: "loading", message: "Loading deterministic fixture…" },
    };
  });
  const [error, setError] = useState<string>();
  // PTY bytes deliberately bypass React state. Routing every output chunk
  // through setState re-rendered the whole deck per chunk per agent — with six
  // streaming agents the main thread spent its time reconciling instead of
  // letting xterm scroll. Buffers live in a ref; terminals subscribe directly.
  const terminalBuffersRef = useRef<Record<string, TerminalBuffer>>({});
  const terminalListenersRef = useRef<Map<string, Set<(buffer: TerminalBuffer) => void>>>(new Map());

  const terminalFeed = useMemo(() => ({
    get: (agentId: string) => terminalBuffersRef.current[agentId],
    subscribe: (agentId: string, listener: (buffer: TerminalBuffer) => void) => {
      const listeners = terminalListenersRef.current.get(agentId) ?? new Set();
      listeners.add(listener);
      terminalListenersRef.current.set(agentId, listeners);
      return () => { listeners.delete(listener); };
    },
  }), []);

  const updateTerminal = useCallback((event: Parameters<typeof applyTerminalChunk>[1]) => {
    const data = event.data.byteLength
      ? event.data
      : event.message
        ? new TextEncoder().encode(`\r\n[terminal] ${event.message}\r\n`)
        : event.data;
    const current = terminalBuffersRef.current[event.agentId];
    const next = applyTerminalChunk(current, { ...event, data });
    if (next === current) return;
    terminalBuffersRef.current[event.agentId] = next;
    for (const listener of terminalListenersRef.current.get(event.agentId) ?? []) listener(next);
  }, []);

  const reconnect = useCallback(async () => {
    setError(undefined);
    setSnapshot((current) => ({ ...current, connection: { ...current.connection, status: "loading", message: "Reconnecting…" } }));
    try {
      const connected = await bridge.connect();
      setSnapshot(connected);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      setSnapshot((current) => ({
        ...current,
        health: "failed",
        connection: { status: "error", message },
      }));
    }
  }, [bridge]);

  useEffect(() => {
    let active = true;
    let unsubscribe: (() => void) | undefined;

    void (async () => {
      try {
        unsubscribe = await bridge.subscribe(
          (next) => active && setSnapshot(next),
          (event) => active && updateTerminal(event),
        );
        if (!active) {
          unsubscribe();
          return;
        }
        const initial = await bridge.connect();
        if (active) setSnapshot(initial);
      } catch (cause) {
        if (!active) return;
        const message = cause instanceof Error ? cause.message : String(cause);
        setError(message);
        setSnapshot((current) => ({ ...current, health: "failed", connection: { status: "error", message } }));
      }
    })();

    return () => {
      active = false;
      unsubscribe?.();
      void bridge.dispose();
    };
  }, [bridge, updateTerminal]);

  const runAction = useCallback(async (action: DeckAction) => {
    setError(undefined);
    try {
      return await bridge.runAction(action);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      throw cause;
    }
  }, [bridge]);

  const sendTerminalInput = useCallback((agentId: string, data: string) => bridge.sendTerminalInput(agentId, data), [bridge]);
  const resizeTerminal = useCallback((agentId: string, cols: number, rows: number) => bridge.resizeTerminal(agentId, cols, rows), [bridge]);

  return {
    mode,
    snapshot,
    terminalData: EMPTY_TERMINAL_DATA,
    terminalFeed,
    error,
    runAction,
    sendTerminalInput,
    resizeTerminal,
    reconnect,
  };
}
