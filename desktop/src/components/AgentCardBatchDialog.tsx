import type { KeyboardEvent, ReactNode } from "react";
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
 * the same exact text (from the same source) or, with no text, the same kind
 * of action. Groups keep the order of their first agent, and agents keep the
 * preview's order within a group.
 */
function sameText(rows: readonly BatchRow[]): BatchRow[][] {
  const groups = new Map<string, BatchRow[]>();
  for (const row of rows) {
    const key = row.entry.prompt !== undefined && row.entry.prompt !== null
      ? `text\u0000${row.entry.prompt_source ?? ""}\u0000${row.entry.prompt}`
      : `kind\u0000${row.entry.kind ?? ""}`;
    groups.set(key, [...(groups.get(key) ?? []), row]);
  }
  return [...groups.values()];
}

/** What an action with no text does to the agents of one group. */
function closeWords(kind: string | undefined, label: string, several: boolean): string {
  if (kind === "close_keep_worktree") return several ? "Closes each of these agents and keeps its worktree." : "Closes this agent and keeps its worktree.";
  if (kind === "close") return several ? "Closes each of these agents." : "Closes this agent.";
  return several ? `Runs ${label} on each of these agents.` : `Runs ${label} on this agent.`;
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
  const previewed = rowsOf(view.groups);
  const included = previewed.filter((row) => row.entry.outcome === "included");
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
        <div className="agent-cards-batch-body">
          {view.stage === "previewing" && <p role="status">Asking the deck what each agent would get…</p>}
          {view.refusal && <p className="agent-cards-batch-refusal" role="status" data-testid="agent-cards-batch-refusal">{displayText(view.refusal, DISPLAY_LIMITS.message)}</p>}
          {view.stage === "sending" && <p role="status">Sending… the result for each agent shows here when every daemon has answered.</p>}
          {view.error && <p className="agent-cards-batch-error" role="alert">Nothing was sent: {displayText(view.error, DISPLAY_LIMITS.message)}</p>}
          {!done && view.stage !== "previewing" && !view.error && (
            <>
              <h3>{included.length === 0 ? "No agent gets this action" : `Sends to ${included.length} ${included.length === 1 ? "agent" : "agents"}`}</h3>
              {included.length > 0 && (
                <ul className="agent-cards-batch-list">
                  {sameText(included).map((group) => {
                    const [first] = group;
                    const what = (several: boolean) => (first.entry.prompt
                      ? <><blockquote className="agent-card-prompt-text agent-cards-batch-prompt" data-testid="agent-cards-batch-prompt"><ExactText text={first.entry.prompt} /></blockquote><ExactTextNote text={first.entry.prompt} /></>
                      : <p className="agent-cards-batch-what">{closeWords(first.entry.kind, label, several)}</p>);
                    const badge = (row: BatchRow) => row.entry.prompt_source === "agent" && <SourceBadge source="agent" />;
                    /* One agent: its row holds the text, as it always did. */
                    if (group.length === 1) {
                      return (
                        <li key={rowKey(first)} className="agent-cards-batch-group" data-testid="agent-cards-batch-group">
                          <div className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                            {who(first, badge(first))}
                            {what(false)}
                          </div>
                        </li>
                      );
                    }
                    /* Several agents get the same text: it is quoted once, then the agents that get it. */
                    return (
                      <li key={rowKey(first)} className="agent-cards-batch-group is-shared" data-testid="agent-cards-batch-group">
                        {what(true)}
                        <p className="agent-cards-batch-gets">{first.entry.prompt ? `Each of these ${group.length} agents gets exactly this text:` : `For each of these ${group.length} agents:`}</p>
                        <ul className="agent-cards-batch-list">
                          {group.map((row) => (
                            <li key={rowKey(row)} className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                              {who(row, badge(row))}
                            </li>
                          ))}
                        </ul>
                      </li>
                    );
                  })}
                </ul>
              )}
              {skipped.length > 0 && (
                <>
                  <h3>Skipped: {skipped.length}</h3>
                  <ul className="agent-cards-batch-list">
                    {skipped.map((row) => (
                      <li key={rowKey(row)} className="agent-cards-batch-entry is-skipped" data-testid="agent-cards-batch-entry">
                        {who(row)}
                        <p className="agent-cards-batch-reason">{displayText(row.entry.message ?? "it is not offered on this card", DISPLAY_LIMITS.message)}</p>
                      </li>
                    ))}
                  </ul>
                </>
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
                <button type="button" className="button secondary" data-testid="agent-cards-batch-cancel" disabled={view.stage === "sending"} onClick={() => VOICE_ACTIONS.closeCardBatch.run(controls)}>Cancel</button>
                <button type="button" className="button primary" data-testid="agent-cards-batch-send" disabled={view.stage !== "review" || included.length === 0} onClick={() => VOICE_ACTIONS.sendCardBatch.run(controls)}>Send</button>
              </>
            )}
        </footer>
      </section>
    </div>
  );
}
