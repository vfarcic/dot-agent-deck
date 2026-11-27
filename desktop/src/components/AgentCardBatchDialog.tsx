import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode, type RefObject } from "react";
import { ChevronRight } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { CardBatchGroup, CardBatchView } from "../hooks/useAgentCardsView";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { deliveryStateWord } from "../lib/agentCards";
import type { CardBatchOutcome } from "../types";
import { ExactText, ExactTextNote, SourceBadge } from "./AgentCard";

/** What one agent's row says it got, after Send. `unknown` is an execution nobody confirmed — the wait ran out (`no_answer`, possibly before the request was written), the connection broke after it was written (`connection_lost`), the webview call rejected (no code), or the daemon started the batch and cannot account for this agent (`interrupted`, `answer_not_kept`): the daemon may or may not have acted, or may still act, so it is neither a failure nor a delivery. A `delivered` entry is worded by its delivery `state` instead ({@link resultWord}). */
const RESULT_WORD: Record<string, string> = { delivered: "Delivered", closed: "Closed", failed: "Failed", unknown: "No answer" };

/**
 * The delivery state a `delivered` entry is worded by. A daemon of this
 * release always says `queued`: the text is in the agent's inbox and the agent
 * has not confirmed it. An entry without a state (an earlier development build
 * of the batch verb) is read the same way, the most that a delivered press can
 * say when it answers, never as the agent's receipt (PR #2491 review).
 */
const deliveredState = (entry: CardBatchOutcome) => entry.state ?? "queued";

/** A `delivered` entry whose state is not the agent's confirmed receipt. */
const isQueued = (entry: CardBatchOutcome) => entry.outcome === "delivered" && deliveredState(entry) !== "delivered";

/**
 * One row's word, after Send. A batch claims no more than a single press: a
 * queued entry reads as the card's own delivery history says the same press
 * ("Queued for the agent"), never "Delivered" (PR #2491 review).
 */
function resultWord(entry: CardBatchOutcome): string {
  if (entry.outcome === "delivered") return deliveryStateWord(deliveredState(entry));
  return RESULT_WORD[entry.outcome] ?? displayText(entry.outcome, DISPLAY_LIMITS.name);
}

/** Whether a preview's action kind closes its agent. */
const isClose = (kind: string | undefined) => kind === "close" || kind === "close_keep_worktree";

/**
 * The headline of the results. Each outcome is counted on its own, in the
 * words of its row: queued for the agent (never delivered: the agent has not
 * confirmed the text yet), delivered to the agent, closed, failed, and not
 * answered. An unanswered agent is counted as neither done nor failed: either
 * may still be true, and what it may still be is the action's (a close may
 * still close it, anything else may still be delivered). `kinds` is each row's
 * action kind, as the preview answered it (PR #2491 review).
 */
function resultsHeadline(rows: readonly BatchRow[], kinds: ReadonlyMap<string, string>): string {
  const total = rows.length;
  const tally = (test: (entry: CardBatchOutcome) => boolean) => rows.filter((row) => test(row.entry)).length;
  const failed = tally((entry) => entry.outcome === "failed");
  const unanswered = rows.filter((row) => row.entry.outcome === "unknown");
  const queued = tally(isQueued);
  const delivered = tally((entry) => entry.outcome === "delivered" && !isQueued(entry));
  const closed = tally((entry) => entry.outcome === "closed");
  const counts = ([[queued, "queued for the agent"], [delivered, "delivered to the agent"], [closed, "closed"]] as const).filter(([count]) => count > 0);
  const done = counts.map(([count, word]) => `${count} ${word}`).join(", ");
  if (unanswered.length > 0) {
    const closing = unanswered.every((row) => isClose(kinds.get(rowKey(row))));
    const parts = [done ? `${done} of ${total}` : `0 of ${total} confirmed ${closing ? "closed" : "sent"}`];
    if (failed > 0) parts.push(`${failed} failed`);
    parts.push(`${unanswered.length} not answered, may still be ${closing ? "closed" : "delivered"}`);
    return parts.join("; ");
  }
  if (failed === 0 && counts.length === 1) {
    const [, word] = counts[0];
    return word === "closed" ? `Closed all ${total}` : word === "delivered to the agent" ? `Delivered to all ${total}` : `Queued for all ${total}`;
  }
  return done ? `${done} of ${total}; ${failed} failed` : `${failed} of ${total} failed`;
}

/**
 * One agent's row, with the daemon it is on. Pane and agent ids are the
 * daemon's own, so two daemons can both have a `pane-1/1`: a row is told apart
 * by its daemon (deck and the daemon process that answered) as well.
 */
interface BatchRow {
  deckId: string;
  instanceId: string;
  entry: CardBatchOutcome;
}

/** A row's identity across the whole batch: its daemon, then the daemon's own ids. */
const rowKey = (row: Pick<BatchRow, "deckId" | "instanceId"> & { entry: Pick<CardBatchOutcome, "pane_id" | "agent_id"> }) =>
  `${row.deckId}\u0000${row.instanceId}\u0000${row.entry.pane_id}\u0000${row.entry.agent_id}`;

/** Every entry of every daemon's part, in order, each still carrying its daemon. */
const rowsOf = (groups: readonly CardBatchGroup[]): BatchRow[] =>
  groups.flatMap((group) => group.entries.map((entry) => ({ deckId: group.deckId, instanceId: group.instanceId, entry })));

/** The name a row shows: the daemon's for the card, else what that same daemon's preview called it, else its pane. */
function rowName(row: BatchRow, names: ReadonlyMap<string, string>): string {
  return displayText(row.entry.name ?? names.get(rowKey(row)) ?? row.entry.pane_id, DISPLAY_LIMITS.name);
}

/**
 * Decision 2026-11-24 — the included rows grouped by what each agent gets:
 * the same exact text, whoever wrote it (issue #2399 review 9: each agent's
 * own row still carries its source badge), or, with no text, the same kind
 * of action. Groups keep the order of their first agent, and agents keep the
 * preview's order within a group.
 */
function sameText(rows: readonly BatchRow[]): BatchRow[][] {
  const groups = new Map<string, BatchRow[]>();
  for (const row of rows) {
    const key = row.entry.prompt !== undefined && row.entry.prompt !== null
      ? `text\u0000${row.entry.prompt}`
      : `kind\u0000${row.entry.kind ?? ""}`;
    groups.set(key, [...(groups.get(key) ?? []), row]);
  }
  return [...groups.values()];
}

/**
 * PR #2491 audit K1 — the names one text group lists: every name, or, folded,
 * only the first {@link INCLUDED_SHOWN}. Folding hides names and nothing else:
 * every distinct text a batch sends is quoted, whatever the list's length.
 */
function foldNames(rows: readonly BatchRow[], open: boolean): readonly BatchRow[] {
  return rows.length > INCLUDED_FOLD_AFTER && !open ? rows.slice(0, INCLUDED_SHOWN) : rows;
}

/**
 * Who wrote a group's text, marked on its quotation: `agent says` when any of
 * its agents' copy is the agent's, else the catalog's (PR #2491 audit K1).
 */
function GroupSource({ rows }: { rows: readonly BatchRow[] }) {
  return rows.some((row) => row.entry.prompt_source === "agent")
    ? <span className="card-source is-agent" data-testid="agent-cards-batch-source" title="Written by the agent; the deck did not check it">agent says</span>
    : <span className="card-source is-catalog" data-testid="agent-cards-batch-source" title="From your catalog">catalog</span>;
}

/** What an action with no text does to the agents of one group. */
function closeWords(kind: string | undefined, label: string, several: boolean): string {
  if (kind === "close_keep_worktree") return several ? "Closes each of these agents and keeps its worktree." : "Closes this agent and keeps its worktree.";
  if (kind === "close") return several ? "Closes each of these agents." : "Closes this agent.";
  return several ? `Runs ${label} on each of these agents.` : `Runs ${label} on this agent.`;
}

/**
 * PR #2491 decision 2026-11-25 — how many names one text group lists before
 * the rest fold behind its own "+N more": the list stays readable with
 * hundreds of agents, and a press shows every one. Texts never fold (audit K1).
 */
const INCLUDED_SHOWN = 8;
/** Past this many agents in one text group, its names past {@link INCLUDED_SHOWN} fold. */
const INCLUDED_FOLD_AFTER = 10;

/**
 * PR #2491 audit K1 (remainder) — how much of a quotation must have been in
 * the body's view for it to count as seen: nearly all of it, or, for a text
 * taller than half the view, at least half the view's height of it.
 */
const SEEN_SHARE = 0.9;
/** The ratios at which the observer reports a quotation, so a tall one is measured as it scrolls through. */
const SEEN_THRESHOLDS = Array.from({ length: 21 }, (_, at) => at / 20);

/** Whether a quotation at `quote` is shown enough inside a body at `root` to count as seen. */
function quotationInView(quote: Pick<DOMRectReadOnly, "top" | "bottom" | "height">, root: Pick<DOMRectReadOnly, "top" | "bottom" | "height">): boolean {
  if (quote.height <= 0 || root.height <= 0) return false;
  const shown = Math.min(quote.bottom, root.bottom) - Math.max(quote.top, root.top);
  return shown > 0 && shown >= Math.min(quote.height * SEEN_SHARE, root.height / 2);
}

/**
 * PR #2491 audit K1 (remainder) — which of the confirmation's distinct texts
 * have been in view at least once, by their key (`keys`, in the order they
 * are quoted). Each quotation carries `data-batch-text={index}`. Watched with
 * an `IntersectionObserver` on the scrolling body where the webview has one,
 * else measured two frames after the dialog opens, the body scrolls, the
 * window resizes or Show next text scrolls, so the frame between has painted
 * what is measured. A quotation counts as seen only there: never in the task
 * that asked for the scroll (PR #2491 final verification P1). While `watch`
 * is false nothing is watched; a new preview starts unseen.
 *
 * PR #2491 observer race: a report is credited only to the preview it was
 * taken for. Each watched node is bound, when it is watched, to the key it
 * quoted then, never looked up again through the current keys (React reuses
 * a row's node for the next preview's text); and a report from a watcher
 * whose preview is no longer the one on screen counts for nothing, even when
 * the browser delivers it after the replacement has rendered and before the
 * old watcher is disconnected.
 */
function useSeenTexts(body: RefObject<HTMLElement | null>, keys: readonly string[], watch: boolean, previewing: boolean) {
  // voice-registry-exempt: which quoted texts the user has had in view, an invariant of the open confirmation and not a control
  const [seen, setSeen] = useState<ReadonlySet<string>>(() => new Set());
  const current = useRef(keys);
  current.current = keys;
  /* The fallback's "measure after the next painted frame", while it is the one watching; null under an observer, which reports after a paint itself. */
  const remeasure = useRef<(() => void) | null>(null);
  const mark = useCallback((key: string) => {
    // voice-registry-exempt: records that a quoted text was in view; nothing is sent or changed
    setSeen((held) => (held.has(key) ? held : new Set([...held, key])));
  }, []);
  useEffect(() => {
    // voice-registry-exempt: a new preview's texts start unseen
    if (previewing) setSeen(new Set());
  }, [previewing]);
  const signature = keys.join("\u0001");
  /* The preview on screen, as last committed: set before any task can deliver a report about the DOM that commit wrote. */
  const shown = useRef<{ signature: string; keys: ReadonlySet<string> } | null>(null);
  useLayoutEffect(() => {
    shown.current = { signature, keys: new Set(keys) };
    /* `keys` is read through the signature it spells: the same signature is the same keys. */
  }, [signature]);
  /* The watcher currently installed, by identity; a disconnected one's late reports are not its successor's. */
  const watcher = useRef<object | null>(null);
  useEffect(() => {
    const root = body.current;
    if (!watch || !root) return;
    const quotes = [...root.querySelectorAll<HTMLElement>("[data-batch-text]")];
    /* Each node is bound now to the key it quotes for this preview, and keeps that key. */
    const keyAt = new WeakMap<Element, string>();
    for (const node of quotes) {
      const key = keys[Number(node.dataset.batchText)];
      if (key !== undefined) keyAt.set(node, key);
    }
    const self = {};
    watcher.current = self;
    /* A node's credit, only while this watcher's preview is the one shown and only to the key it was bound to. */
    const credit = (node: Element) => {
      if (watcher.current !== self || shown.current?.signature !== signature) return;
      const key = keyAt.get(node);
      if (key !== undefined && shown.current.keys.has(key)) mark(key);
    };
    const retire = () => {
      if (watcher.current === self) watcher.current = null;
    };
    if (typeof IntersectionObserver === "function") {
      const observer = new IntersectionObserver((entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting && quotationInView(entry.boundingClientRect, entry.rootBounds ?? root.getBoundingClientRect())) credit(entry.target);
        }
      }, { root, threshold: SEEN_THRESHOLDS });
      quotes.forEach((node) => observer.observe(node));
      return () => {
        retire();
        observer.disconnect();
      };
    }
    const measure = () => {
      const bounds = root.getBoundingClientRect();
      for (const node of quotes) {
        if (quotationInView(node.getBoundingClientRect(), bounds)) credit(node);
      }
    };
    /* Two frames after the last change: the first paints it, the second
       measures what that paint showed. A later change starts the wait again,
       so a position is measured only once it has been on screen. */
    let frame = 0;
    const later = () => {
      window.cancelAnimationFrame(frame);
      frame = window.requestAnimationFrame(() => {
        frame = window.requestAnimationFrame(() => {
          frame = 0;
          measure();
        });
      });
    };
    later();
    remeasure.current = later;
    root.addEventListener("scroll", later, { passive: true });
    window.addEventListener("resize", later);
    return () => {
      retire();
      window.cancelAnimationFrame(frame);
      remeasure.current = null;
      root.removeEventListener("scroll", later);
      window.removeEventListener("resize", later);
    };
    /* `keys` is read through the signature it spells: the same signature is the same keys. */
  }, [body, mark, signature, watch]);
  /**
   * Scroll the first text not yet seen into view, and only that: it counts as
   * seen when the observer or the fallback's measurement finds it in view
   * after a paint, so pressing this several times in one go enables nothing.
   */
  const showNext = useCallback(() => {
    const at = current.current.findIndex((key) => !seen.has(key));
    if (at < 0) return;
    body.current?.querySelector<HTMLElement>(`[data-batch-text="${at}"]`)?.scrollIntoView?.({ block: "center" });
    remeasure.current?.();
  }, [body, seen]);
  return { seen, showNext };
}

/**
 * Why a group of skipped agents is skipped, in a few words after its count
 * ("150 don't offer Go ahead"), by the code the daemon skipped them with. A
 * code this build does not know reads "for another reason"; each agent's row
 * in the expanded list still carries the daemon's own words.
 */
function skipWords(code: string | undefined, label: string, count: number): string {
  const one = count === 1;
  switch (code) {
    case "unknown_action": return one ? `doesn't offer ${label}` : `don't offer ${label}`;
    case "agent_stopped": return one ? "isn't running" : "aren't running";
    case "action_disabled": return one ? `can't use ${label} now` : `can't use ${label} now`;
    case "text_required": return one ? "needs your own words" : "need your own words";
    case "client_side_action": return one ? "is opened one card at a time" : "are opened one card at a time";
    case "daemon_unreachable": return one ? "is on a daemon that did not answer" : "are on a daemon that did not answer";
    case "unknown_entry": case "stale_entry": return one ? "is no longer listed" : "are no longer listed";
    default: return "for another reason";
  }
}

/** The skipped rows grouped by the code they were skipped with, in the order each code is first met. */
function byReason(rows: readonly BatchRow[]): { code: string; rows: BatchRow[] }[] {
  const groups = new Map<string, BatchRow[]>();
  for (const row of rows) {
    const code = row.entry.code && skipWords(row.entry.code, "", 2) !== "for another reason" ? row.entry.code : "other";
    groups.set(code, [...(groups.get(code) ?? []), row]);
  }
  return [...groups.entries()].map(([code, grouped]) => ({ code, rows: grouped }));
}

/**
 * Issue #2399 — the batch confirmation, then its result per agent. It lists
 * exactly what the daemons' preview answered: each included agent with the
 * exact text it gets (quoted, never an input) — a text several agents get is
 * quoted once, above the agents that get it (decision 2026-11-24) — and each
 * skipped agent with the reason. Every batch stops here, whatever the action's own `confirm`
 * says, and only Send sends; voice opens this and never presses it (PRD #802
 * D5). After Send it says, for each agent, whether its text was queued for it
 * (the most a press can say when it answers: the card's delivery history
 * shows when the agent confirms it) or why it failed — a partial failure
 * reads as one, never as a success. A daemon
 * that did not answer before the desktop stopped waiting leaves its agents
 * "No answer": the daemon may still deliver, so they read as neither.
 *
 * Every row keeps its daemon (#2399 review 1), and names it when the batch
 * reaches more than one. Cancel, Close, `Escape` and voice's `close` dismiss
 * it, all through `closeCardBatch`, which refuses while the batch is being
 * sent, so none of them throws its results away (#2399 review 2 and 4).
 */
export function AgentCardBatchDialog({ view, controls, daemonName }: {
  view: CardBatchView;
  controls: Pick<VoiceActionContext, "sendCardBatch" | "closeCardBatch">;
  /** The name the dashboard gives a deck's daemon, for a batch that reaches more than one. */
  daemonName: (deckId: string) => string;
}) {
  /* Voice's `close` reaches this through the overlay stack; while the batch is
     being sent it is refused, and the refusal is what `close` says. */
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => controls.closeCardBatch());
  /* The two disclosures (PR #2491 decision 2026-11-25): the skipped agents' list and the included names past the first few. */
  // voice-registry-exempt: whether the confirmation lists every skipped agent, a disclosure of what the open dialog already counts
  const [skippedOpen, setSkippedOpen] = useState(false);
  // voice-registry-exempt: which text groups list every included agent's name, a disclosure of what the open dialog already counts
  const [openGroups, setOpenGroups] = useState<ReadonlySet<string>>(() => new Set());
  const skippedListId = useId();
  const includedListId = useId();
  const previewed = rowsOf(view.groups);
  const included = previewed.filter((row) => row.entry.outcome === "included");
  const textGroups = sameText(included);
  const quoted = textGroups.filter(([first]) => first.entry.prompt !== undefined && first.entry.prompt !== null);
  const distinctTexts = quoted.length;
  /* PR #2491 audit K1 (remainder): with more than one distinct text, Send
     waits until each has been in view at least once. One text — however many
     agents share it — is in view with the heading, as before. */
  const textKeys = quoted.map(([first]) => `${rowKey(first)}\u0000${first.entry.prompt}`);
  const textIndex = new Map(quoted.map(([first], index) => [rowKey(first), index]));
  const body = useRef<HTMLDivElement>(null);
  const gated = view.stage === "review" && distinctTexts > 1;
  const { seen, showNext } = useSeenTexts(body, textKeys, gated, view.stage === "previewing");
  const unseen = gated ? textKeys.filter((key) => !seen.has(key)).length : 0;
  const toggleGroup = (groupKey: string) => setOpenGroups((current) => {
    const next = new Set(current);
    if (!next.delete(groupKey)) next.add(groupKey);
    return next;
  });
  const skipped = previewed.filter((row) => row.entry.outcome !== "included");
  const names = new Map(previewed.flatMap((row) => (row.entry.name ? [[rowKey(row), row.entry.name] as const] : [])));
  const kinds = new Map(previewed.flatMap((row) => (row.entry.kind ? [[rowKey(row), row.entry.kind] as const] : [])));
  const done = view.stage === "done";
  const results = rowsOf(view.results);
  /* Which daemon each agent is on, said only when the batch reaches more than one. */
  const severalDaemons = new Set([...view.groups, ...view.results].map((group) => group.deckId)).size > 1;
  const who = (row: BatchRow, extra?: ReactNode) => (
    <p className="agent-cards-batch-who">
      <strong className="agent-cards-batch-name">{rowName(row, names)}</strong>
      {severalDaemons && <span className="agent-cards-batch-daemon" data-testid="agent-cards-batch-daemon">on {displayText(daemonName(row.deckId), DISPLAY_LIMITS.name)}</span>}
      {extra}
    </p>
  );
  const onKeyDown = (event: KeyboardEvent<HTMLElement>) => {
    if (event.key !== "Escape" || event.metaKey || event.ctrlKey || event.altKey) return;
    event.preventDefault();
    event.stopPropagation();
    VOICE_ACTIONS.closeCardBatch.run(controls);
  };
  const label = displayText(view.label, DISPLAY_LIMITS.name);
  const scope = displayText(view.scope, DISPLAY_LIMITS.message);
  return (
    <div className="dialog-backdrop agent-cards-batch-backdrop" role="presentation">
      <section
        ref={ref}
        tabIndex={-1}
        className="agent-cards-batch"
        role="dialog"
        aria-modal="true"
        aria-labelledby="agent-cards-batch-title"
        data-testid={done ? "agent-cards-batch-results" : "agent-cards-batch-confirm"}
        data-stage={view.stage}
        onKeyDown={onKeyDown}
      >
        <header className="agent-cards-batch-header">
          <h2 id="agent-cards-batch-title">{done ? `${label}: what happened` : `${label}?`}</h2>
          <p className="agent-cards-batch-scope">{scope}</p>
        </header>
        <div ref={body} className="agent-cards-batch-body" data-testid="agent-cards-batch-body">
          {view.stage === "previewing" && <p role="status">Asking the deck what each agent would get…</p>}
          {view.refusal && <p className="agent-cards-batch-refusal" role="status" data-testid="agent-cards-batch-refusal">{displayText(view.refusal, DISPLAY_LIMITS.message)}</p>}
          {view.stage === "sending" && <p role="status">Sending… the result for each agent shows here when every daemon has answered.</p>}
          {view.error && <p className="agent-cards-batch-error" role="alert">Nothing was sent: {displayText(view.error, DISPLAY_LIMITS.message)}</p>}
          {!done && view.stage !== "previewing" && !view.error && (
            <>
              <h3>{included.length === 0 ? "No agent gets this action" : `Sends to ${included.length} ${included.length === 1 ? "agent" : "agents"}${distinctTexts > 1 ? `: ${distinctTexts} different texts` : ""}`}</h3>
              {included.length > 0 && (
                <ul className="agent-cards-batch-list" id={includedListId}>
                  {textGroups.map((group, index) => {
                    const [first] = group;
                    const groupKey = rowKey(first);
                    const listId = `${includedListId}-${index}`;
                    const open = openGroups.has(groupKey);
                    const listed = foldNames(group, open);
                    /* Every distinct text is quoted with who wrote it, however long the list (audit K1). */
                    const what = (several: boolean) => (first.entry.prompt
                      ? <><p className="agent-cards-batch-text-source"><GroupSource rows={group} /></p><blockquote className="agent-card-prompt-text agent-cards-batch-prompt" data-testid="agent-cards-batch-prompt" data-batch-text={textIndex.get(groupKey)}><ExactText text={first.entry.prompt} /></blockquote><ExactTextNote text={first.entry.prompt} /></>
                      : <p className="agent-cards-batch-what">{closeWords(first.entry.kind, label, several)}</p>);
                    const badge = (row: BatchRow) => row.entry.prompt_source === "agent" && <SourceBadge source="agent" />;
                    /* One agent: its row holds the text, as it always did; the quotation's mark says who wrote it. */
                    if (group.length === 1) {
                      return (
                        <li key={groupKey} className="agent-cards-batch-group" data-testid="agent-cards-batch-group">
                          <div className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                            {who(first, first.entry.prompt ? undefined : badge(first))}
                            {what(false)}
                          </div>
                        </li>
                      );
                    }
                    /* Several agents get the same text: it is quoted once, then the agents that get it, folded past the first few. */
                    return (
                      <li key={groupKey} className="agent-cards-batch-group is-shared" data-testid="agent-cards-batch-group">
                        {what(true)}
                        <p className="agent-cards-batch-gets">{first.entry.prompt ? `Each of these ${group.length} agents gets exactly this text:` : `For each of these ${group.length} agents:`}</p>
                        <ul className="agent-cards-batch-list" id={listId}>
                          {listed.map((row) => (
                            <li key={rowKey(row)} className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                              {who(row, badge(row))}
                            </li>
                          ))}
                        </ul>
                        {group.length > INCLUDED_FOLD_AFTER && (
                          <button type="button" className="agent-cards-batch-more" data-testid="agent-cards-batch-more" aria-expanded={open} aria-controls={listId} onClick={() => toggleGroup(groupKey)}>
                            {open ? "Show fewer" : `+${group.length - INCLUDED_SHOWN} more`}
                          </button>
                        )}
                      </li>
                    );
                  })}
                </ul>
              )}
              {skipped.length > 0 && (
                <section className="agent-cards-batch-skipped" aria-label="Skipped agents">
                  <button type="button" className="agent-cards-batch-skipped-toggle" data-testid="agent-cards-batch-skipped-toggle" aria-expanded={skippedOpen} aria-controls={skippedListId} onClick={() => setSkippedOpen((open) => !open)}>
                    <ChevronRight className="agent-cards-batch-chevron" size={14} aria-hidden="true" />
                    <span>{`${skipped.length} skipped: ${byReason(skipped).map((group) => `${group.rows.length} ${skipWords(group.code, label, group.rows.length)}`).join(", ")}`}</span>
                  </button>
                  {skippedOpen && (
                    <div id={skippedListId} className="agent-cards-batch-skipped-list">
                      {byReason(skipped).map((group) => (
                        <section key={group.code} className="agent-cards-batch-skipped-group" data-testid="agent-cards-batch-skipped-group" data-reason={group.code}>
                          <h4>{`${group.rows.length} ${skipWords(group.code, label, group.rows.length)}`}</h4>
                          <ul className="agent-cards-batch-list">
                            {group.rows.map((row) => (
                              <li key={rowKey(row)} className="agent-cards-batch-entry is-skipped" data-testid="agent-cards-batch-entry">
                                {who(row)}
                                <p className="agent-cards-batch-reason">{displayText(row.entry.message ?? "it is not offered on this card", DISPLAY_LIMITS.message)}</p>
                              </li>
                            ))}
                          </ul>
                        </section>
                      ))}
                    </div>
                  )}
                </section>
              )}
            </>
          )}
          {done && (
            <>
              <h3 role="status">{resultsHeadline(results, kinds)}</h3>
              <ul className="agent-cards-batch-list">
                {results.map((row) => { const { entry } = row; return (
                  <li key={rowKey(row)} className={`agent-cards-batch-entry is-${entry.outcome === "failed" ? "failed" : entry.outcome === "unknown" ? "unknown" : "done"}`} data-testid="agent-cards-batch-entry" data-outcome={entry.outcome}>
                    {who(row, <span className="agent-cards-batch-outcome">{resultWord(entry)}</span>)}
                    {entry.outcome === "failed" && <p className="agent-cards-batch-reason">{displayText(entry.message ?? "the deck did not act on this agent", DISPLAY_LIMITS.message)}</p>}
                    {entry.outcome === "unknown" && <p className="agent-cards-batch-reason">{displayText(entry.message ?? "the deck did not answer, so it is not known whether this agent got it", DISPLAY_LIMITS.message)}</p>}
                  </li>
                ); })}
              </ul>
              {skipped.length > 0 && <p className="agent-card-hint">Skipped, as the confirmation said: {skipped.length}.</p>}
            </>
          )}
        </div>
        <footer className="agent-cards-batch-buttons">
          {done
            ? <button type="button" className="button primary" data-testid="agent-cards-batch-close" onClick={() => VOICE_ACTIONS.closeCardBatch.run(controls)}>Close</button>
            : (
              <>
                {unseen > 0 && (
                  <>
                    <p id="agent-cards-batch-unseen" className="agent-cards-batch-unseen" role="status" data-testid="agent-cards-batch-unseen">{`Scroll to see all ${distinctTexts} texts`}</p>
                    <button type="button" className="button secondary" data-testid="agent-cards-batch-next-text" onClick={showNext}>Show next text</button>
                  </>
                )}
                <button type="button" className="button secondary" data-testid="agent-cards-batch-cancel" disabled={view.stage === "sending"} onClick={() => VOICE_ACTIONS.closeCardBatch.run(controls)}>Cancel</button>
                <button type="button" className="button primary" data-testid="agent-cards-batch-send" disabled={view.stage !== "review" || included.length === 0 || unseen > 0} aria-describedby={unseen > 0 ? "agent-cards-batch-unseen" : undefined} onClick={() => VOICE_ACTIONS.sendCardBatch.run(controls)}>Send</button>
              </>
            )}
        </footer>
      </section>
    </div>
  );
}
