import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { DeckBridge } from "../lib/bridge";
import { isInterruptedList, listedCount, type DismissOutcome, type InterruptedList, type InterruptedReply, type InterruptedShow, type InterruptedTarget, type ReclaimOutcome, type RestoreMode, type RestoreOutcome, type StartOverOutcome } from "../lib/interruptedWork";
import type { DeckFleet } from "../types";

/** PRD #1790 stage 2 — what the app holds of one deck's interrupted work. */
export interface InterruptedDeck {
  /** The daemon process the deck's connection names: every request is pinned to it. */
  instance: string;
  /** The last list that process answered; absent before an answer, and when it does not support interrupted work. */
  list?: InterruptedList;
  /** Why the last list request failed, for the dialog. */
  failed?: string;
  /** The process answered that it does not support interrupted work. */
  unsupported?: true;
  /**
   * PRD #1790 stage 3 — the process answered a restore or start-over that it
   * does not restore agents: Restore then says so without asking again, as
   * the TUI's does for its daemon.
   */
  restoreUnsupported?: true;
}

/**
 * What one list request found (final reviewer 2): how many items the process
 * lists and whether its list is incomplete (verification finding C: the
 * process could not read its whole journal, so an empty list is not an answer
 * that there is none), that it does not support interrupted work, that the
 * request failed, or that its answer was not applied because the deck left
 * that process or a newer request replaced it.
 */
type InterruptedFetch =
  | { outcome: "listed"; count: number; incomplete: boolean }
  | { outcome: "unsupported" }
  | { outcome: "failed"; reason: string }
  | { outcome: "superseded" };

/** The deck and daemon process the dialog was opened for. */
interface InterruptedDialog { deckId: string; instance: string }

/** PRD #1790 stage 2 — the app's interrupted work, every deck's, and the calls that act on it. */
export interface InterruptedWorkRuntime {
  /** By deck id: each connected deck whose daemon is asked. */
  decks: Readonly<Record<string, InterruptedDeck>>;
  /** Whether the banner of deck `deckId` was hidden for its current daemon process. */
  hidden: (deckId: string) => boolean;
  /** Hide deck `deckId`'s banner: records nothing, and lasts until its connection names another daemon process. */
  hide: (deckId: string) => void;
  /** Bring back the hidden banners of decks `deckIds`, so a user told to open one from its banner has one to open (verification R2). */
  reveal: (deckIds: readonly string[]) => void;
  /**
   * Ask deck `deckId`'s daemon for the list again; `git` asks for each item's
   * Git and pull request facts. Settles with what that request found, once its
   * answer is applied, so a caller can decide from the fresh answer rather
   * than from the list held before it.
   */
  refresh: (deckId: string, git?: boolean) => Promise<InterruptedFetch>;
  show: (target: InterruptedTarget, id: string, reportsFrom: number) => Promise<InterruptedReply<InterruptedShow>>;
  /**
   * A dry run, or a real reclaim. A real one, like a dismissal, is held here
   * rather than by the dialog (reviewer 2): it stays pending while the dialog
   * is closed and opened again ({@link busy}), and when it answers, the list
   * is asked for again if the deck still names the process it was sent to.
   */
  reclaim: (target: InterruptedTarget, id: string, dryRun: boolean) => Promise<InterruptedReply<ReclaimOutcome>>;
  dismiss: (target: InterruptedTarget, ids: string[]) => Promise<InterruptedReply<DismissOutcome>>;
  /**
   * PRD #1790 stage 3 — restore attempt `attempt` of item `id` with its
   * conversation, or start it over without it. Held here like a dismissal:
   * pending ({@link busy}) until the daemon answers — once the agent
   * confirmed its conversation, or was stopped — then the list again. The
   * daemon starts the agent; the app starts nothing itself.
   */
  restore: (target: InterruptedTarget, id: string, attempt: string, mode: RestoreMode) => Promise<InterruptedReply<RestoreOutcome>>;
  startOver: (target: InterruptedTarget, id: string, attempt: string) => Promise<InterruptedReply<StartOverOutcome>>;
  /** Whether a real reclaim, a dismissal, a restore or a start-over sent to `target`'s daemon process is still waiting for its answer. */
  busy: (target: InterruptedTarget) => boolean;
  /** The open "Interrupted work" dialog, when one is. */
  dialog?: InterruptedDialog;
  /** Open deck `deckId`'s dialog; `false` when that deck has no daemon process to ask. */
  openDialog: (deckId: string) => boolean;
  closeDialog: () => void;
}

/** A message from a rejected bridge call. */
function reason(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * PRD #1790 stage 2 — each deck's interrupted work.
 *
 * The list is asked for once per daemon process a deck's connection names —
 * when the deck connects, and again when it reconnects to another process —
 * again when its dialog opens (with the Git facts), when the person asks to
 * see interrupted work without naming a daemon while several are connected
 * (final reviewer 2: `refresh`, so the dashboard decides from fresh answers),
 * and after each reclaim or dismissal — whether or not a dialog is still open
 * to show its answer. Nothing polls on a timer. An answer from a process the deck has
 * since left, or to a list request that a newer one replaced, is dropped.
 *
 * `undefined` where the bridge offers none of it (a render-only runtime).
 */
export function useInterruptedWork(bridge: DeckBridge, fleet: DeckFleet): InterruptedWorkRuntime | undefined {
  const offered = typeof bridge.interruptedWorkList === "function";
  const [decks, setDecks] = useState<Readonly<Record<string, InterruptedDeck>>>({});
  /* Which deck's banner Hide was pressed on, by the daemon process it was pressed for. */
  const [hiddenFor, setHiddenFor] = useState<Readonly<Record<string, string>>>({});
  const [dialog, setDialog] = useState<InterruptedDialog | undefined>();
  /* The process each connected deck names now, as of the last fleet seen. */
  const current = useRef(new Map<string, string>());
  /* The newest list request per deck: an older one's answer is not applied. */
  const lastOp = useRef(0);
  const listOps = useRef(new Map<string, number>());
  /* The real reclaim or dismissal in flight per deck: the process it was sent to and its op. A ref for the
     synchronous check, mirrored in state so a dialog renders it. */
  const mutations = useRef(new Map<string, { instance: string; op: number }>());
  const [mutating, setMutating] = useState<Readonly<Record<string, string>>>({});

  const fetchList = useCallback((deckId: string, git: boolean): Promise<InterruptedFetch> => {
    const superseded: InterruptedFetch = { outcome: "superseded" };
    if (!bridge.interruptedWorkList) return Promise.resolve(superseded);
    const instance = current.current.get(deckId);
    if (instance === undefined) return Promise.resolve(superseded);
    const op = ++lastOp.current;
    listOps.current.set(deckId, op);
    const applies = () => current.current.get(deckId) === instance && listOps.current.get(deckId) === op;
    return bridge.interruptedWorkList({ deckId, instance }, git).then((reply): InterruptedFetch => {
      if (!applies()) return superseded;
      const listed = reply.outcome === "answered" && isInterruptedList(reply.answer);
      setDecks((held) => {
        const before = held[deckId];
        if (!before || before.instance !== instance) return held;
        if (reply.outcome !== "answered" || !isInterruptedList(reply.answer)) return { ...held, [deckId]: reply.outcome === "unsupported" ? { instance, unsupported: true } : { instance } };
        /* A refresh without Git facts keeps the ones a list with them read, for the items still listed. */
        const items = reply.answer.items.map((item) => (item.git ? item : { ...item, git: before.list?.items.find((old) => old.id === item.id)?.git ?? item.git }));
        return { ...held, [deckId]: { instance, list: { ...reply.answer, items }, ...(before.restoreUnsupported ? { restoreUnsupported: true as const } : {}) } };
      });
      if (listed) {
        const list = reply.answer as InterruptedList;
        return { outcome: "listed", count: listedCount(list), incomplete: list.incomplete === true };
      }
      return reply.outcome === "unsupported" ? { outcome: "unsupported" } : { outcome: "failed", reason: "the daemon's answer could not be read" };
    }, (error: unknown): InterruptedFetch => {
      if (!applies()) return superseded;
      /* A failed refresh keeps the list it had. */
      setDecks((held) => (held[deckId]?.instance === instance ? { ...held, [deckId]: { ...held[deckId], failed: reason(error) } } : held));
      return { outcome: "failed", reason: reason(error) };
    });
  }, [bridge]);

  useEffect(() => {
    if (!offered) return;
    const next = new Map<string, string>();
    for (const deck of fleet) {
      if (deck.connection.deckId === undefined || deck.connection.status !== "connected") continue;
      next.set(deck.connection.deckId, deck.connection.incarnation ?? "");
    }
    const before = current.current;
    current.current = next;
    const arrived = [...next].filter(([deckId, instance]) => before.get(deckId) !== instance).map(([deckId]) => deckId);
    setDecks((held) => {
      let changed = false;
      const kept: Record<string, InterruptedDeck> = {};
      for (const [deckId, instance] of next) {
        const was = held[deckId];
        if (was && was.instance === instance) kept[deckId] = was;
        else {
          kept[deckId] = { instance };
          changed = true;
        }
      }
      if (Object.keys(held).some((deckId) => !next.has(deckId))) changed = true;
      return changed ? kept : held;
    });
    setDialog((open) => (open && next.get(open.deckId) !== open.instance ? undefined : open));
    for (const deckId of arrived) void fetchList(deckId, false);
  }, [fetchList, fleet, offered]);

  const refresh = useCallback((deckId: string, git = false) => fetchList(deckId, git), [fetchList]);
  const hide = useCallback((deckId: string) => {
    const instance = current.current.get(deckId);
    if (instance !== undefined) setHiddenFor((held) => ({ ...held, [deckId]: instance }));
  }, []);
  const reveal = useCallback((deckIds: readonly string[]) => {
    setHiddenFor((held) => {
      if (!deckIds.some((deckId) => deckId in held)) return held;
      const rest = { ...held };
      for (const deckId of deckIds) delete rest[deckId];
      return rest;
    });
  }, []);
  const hidden = useCallback((deckId: string) => {
    const instance = current.current.get(deckId);
    return instance !== undefined && hiddenFor[deckId] === instance;
  }, [hiddenFor]);
  const openDialog = useCallback((deckId: string) => {
    const instance = current.current.get(deckId);
    if (instance === undefined) return false;
    setDialog({ deckId, instance });
    void fetchList(deckId, true);
    return true;
  }, [fetchList]);
  const closeDialog = useCallback(() => setDialog(undefined), []);
  const show = useCallback((target: InterruptedTarget, id: string, reportsFrom: number) => (
    bridge.interruptedWorkShow ? bridge.interruptedWorkShow(target, id, reportsFrom) : Promise.resolve<InterruptedReply<InterruptedShow>>({ outcome: "unsupported" })
  ), [bridge]);
  /* A real reclaim or dismissal: pending until it answers, then the list again, of that process only. */
  const mutate = useCallback(<T,>(target: InterruptedTarget, send: () => Promise<T>): Promise<T> => {
    const op = ++lastOp.current;
    mutations.current.set(target.deckId, { instance: target.instance, op });
    setMutating((held) => ({ ...held, [target.deckId]: target.instance }));
    const settled = () => {
      if (mutations.current.get(target.deckId)?.op === op) {
        mutations.current.delete(target.deckId);
        setMutating((held) => {
          const rest = { ...held };
          delete rest[target.deckId];
          return rest;
        });
      }
      if (current.current.get(target.deckId) === target.instance) void fetchList(target.deckId, false);
    };
    return send().then((reply) => {
      settled();
      return reply;
    }, (error: unknown) => {
      settled();
      throw error;
    });
  }, [fetchList]);
  const busy = useCallback((target: InterruptedTarget) => mutations.current.get(target.deckId)?.instance === target.instance || mutating[target.deckId] === target.instance, [mutating]);
  const reclaim = useCallback((target: InterruptedTarget, id: string, dryRun: boolean) => {
    const send = () => (bridge.interruptedWorkReclaim ? bridge.interruptedWorkReclaim(target, id, dryRun) : Promise.resolve<InterruptedReply<ReclaimOutcome>>({ outcome: "unsupported" }));
    return dryRun ? send() : mutate(target, send);
  }, [bridge, mutate]);
  const dismiss = useCallback((target: InterruptedTarget, ids: string[]) => mutate(target, () => (
    bridge.interruptedWorkDismiss ? bridge.interruptedWorkDismiss(target, ids) : Promise.resolve<InterruptedReply<DismissOutcome>>({ outcome: "unsupported" })
  )), [bridge, mutate]);

  /* A restore or start-over answered `unsupported`: that process does not restore agents. */
  const learnUnsupported = useCallback(<T,>(target: InterruptedTarget, reply: InterruptedReply<T>): InterruptedReply<T> => {
    if (reply.outcome === "unsupported") {
      setDecks((held) => (held[target.deckId]?.instance === target.instance ? { ...held, [target.deckId]: { ...held[target.deckId], restoreUnsupported: true } } : held));
    }
    return reply;
  }, []);
  const restore = useCallback((target: InterruptedTarget, id: string, attempt: string, mode: RestoreMode) => mutate(target, () => (
    bridge.interruptedWorkRestore ? bridge.interruptedWorkRestore(target, id, [attempt], mode, false) : Promise.resolve<InterruptedReply<RestoreOutcome>>({ outcome: "unsupported" })
  )).then((reply) => learnUnsupported(target, reply)), [bridge, learnUnsupported, mutate]);
  const startOver = useCallback((target: InterruptedTarget, id: string, attempt: string) => mutate(target, () => (
    bridge.interruptedWorkStartOver ? bridge.interruptedWorkStartOver(target, id, attempt, false) : Promise.resolve<InterruptedReply<StartOverOutcome>>({ outcome: "unsupported" })
  )).then((reply) => learnUnsupported(target, reply)), [bridge, learnUnsupported, mutate]);

  return useMemo(() => (offered ? { decks, hidden, hide, reveal, refresh, show, reclaim, dismiss, restore, startOver, busy, ...(dialog ? { dialog } : {}), openDialog, closeDialog } : undefined), [busy, closeDialog, decks, dialog, dismiss, hidden, hide, offered, openDialog, reclaim, refresh, restore, reveal, show, startOver]);
}
