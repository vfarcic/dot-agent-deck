import type { KeyboardEvent, ReactNode } from "react";
import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { CardBatchGroup, CardBatchView } from "../hooks/useAgentCardsView";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { CardBatchOutcome } from "../types";
import { ExactText, ExactTextNote, SourceBadge } from "./AgentCard";

/** What one agent's row says it got, after Send. `unknown` is a request that was sent and then not answered — the daemon did not answer in time, or the connection broke: it may have acted or still act, so it is neither a failure nor a delivery. */
const RESULT_WORD: Record<string, string> = { delivered: "Delivered", closed: "Closed", failed: "Failed", unknown: "No answer" };

/** The headline of the results. Unanswered agents are not counted as sent or as failed: either may still be true. */
function resultsHeadline(total: number, failed: number, unknown: number): string {
  if (unknown === 0) return failed === 0 ? `Sent to all ${total}` : `${total - failed} of ${total} sent; ${failed} failed`;
  const parts = [`${total - failed - unknown} of ${total} confirmed sent`];
  if (failed > 0) parts.push(`${failed} failed`);
  parts.push(`${unknown} not answered, may still be delivered`);
  return parts.join("; ");
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
 * Issue #2399 — the batch confirmation, then its result per agent. It lists
 * exactly what the daemons' preview answered: each included agent with the
 * exact text it gets (quoted, never an input), and each skipped agent with
 * the reason. Every batch stops here, whatever the action's own `confirm`
 * says, and only Send sends; voice opens this and never presses it (PRD #802
 * D5). After Send it says, for each agent, whether its text was delivered or
 * why it failed — a partial failure reads as one, never as a success. A daemon
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
  const done = view.stage === "done";
  const results = rowsOf(view.results);
  const failed = results.filter((row) => row.entry.outcome === "failed").length;
  const unknown = results.filter((row) => row.entry.outcome === "unknown").length;
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
                  {included.map((row) => { const { entry } = row; return (
                    <li key={rowKey(row)} className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                      {who(row, entry.prompt_source === "agent" && <SourceBadge source="agent" />)}
                      {entry.prompt
                        ? <><blockquote className="agent-card-prompt-text agent-cards-batch-prompt" data-testid="agent-cards-batch-prompt"><ExactText text={entry.prompt} /></blockquote><ExactTextNote text={entry.prompt} /></>
                        : <p className="agent-cards-batch-what">{entry.kind === "close_keep_worktree" ? "Closes this agent and keeps its worktree." : entry.kind === "close" ? "Closes this agent." : `Runs ${label} on this agent.`}</p>}
                    </li>
                  ); })}
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
              <h3 role="status">{resultsHeadline(results.length, failed, unknown)}</h3>
              <ul className="agent-cards-batch-list">
                {results.map((row) => { const { entry } = row; return (
                  <li key={rowKey(row)} className={`agent-cards-batch-entry is-${entry.outcome === "failed" ? "failed" : entry.outcome === "unknown" ? "unknown" : "done"}`} data-testid="agent-cards-batch-entry" data-outcome={entry.outcome}>
                    {who(row, <span className="agent-cards-batch-outcome">{RESULT_WORD[entry.outcome] ?? displayText(entry.outcome, DISPLAY_LIMITS.name)}</span>)}
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
