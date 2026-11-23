import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { CardBatchGroup, CardBatchView } from "../hooks/useAgentCardsView";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { CardBatchOutcome } from "../types";
import { ExactText, ExactTextNote, SourceBadge } from "./AgentCard";

/** What one agent's row says it got, after Send. */
const RESULT_WORD: Record<string, string> = { delivered: "Delivered", closed: "Closed", failed: "Failed" };

/** The name a row shows: the daemon's for the card, else its pane. */
function rowName(entry: CardBatchOutcome, names: ReadonlyMap<string, string>): string {
  return displayText(entry.name ?? names.get(`${entry.pane_id}\u0000${entry.agent_id}`) ?? entry.pane_id, DISPLAY_LIMITS.name);
}

/** Every entry of every daemon's part, in order. */
const entriesOf = (groups: readonly CardBatchGroup[]) => groups.flatMap((group) => group.entries);

/**
 * Issue #2399 — the batch confirmation, then its result per agent. It lists
 * exactly what the daemons' preview answered: each included agent with the
 * exact text it gets (quoted, never an input), and each skipped agent with
 * the reason. Every batch stops here, whatever the action's own `confirm`
 * says, and only Send sends; voice opens this and never presses it (PRD #802
 * D5). After Send it says, for each agent, whether its text was delivered or
 * why it failed — a partial failure reads as one, never as a success.
 */
export function AgentCardBatchDialog({ view, controls }: {
  view: CardBatchView;
  controls: Pick<VoiceActionContext, "sendCardBatch" | "closeCardBatch">;
}) {
  const ref = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => { controls.closeCardBatch(); });
  const previewed = entriesOf(view.groups);
  const included = previewed.filter((entry) => entry.outcome === "included");
  const skipped = previewed.filter((entry) => entry.outcome !== "included");
  const names = new Map(previewed.flatMap((entry) => (entry.name ? [[`${entry.pane_id}\u0000${entry.agent_id}`, entry.name] as const] : [])));
  const done = view.stage === "done";
  const results = entriesOf(view.results);
  const failed = results.filter((entry) => entry.outcome === "failed").length;
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
      >
        <header className="agent-cards-batch-header">
          <h2 id="agent-cards-batch-title">{done ? `${label}: what happened` : `${label}?`}</h2>
          <p className="agent-cards-batch-scope">{scope}</p>
        </header>
        <div className="agent-cards-batch-body">
          {view.stage === "previewing" && <p role="status">Asking the deck what each agent would get…</p>}
          {view.error && <p className="agent-cards-batch-error" role="alert">Nothing was sent: {displayText(view.error, DISPLAY_LIMITS.message)}</p>}
          {!done && view.stage !== "previewing" && !view.error && (
            <>
              <h3>{included.length === 0 ? "No agent gets this action" : `Sends to ${included.length} ${included.length === 1 ? "agent" : "agents"}`}</h3>
              {included.length > 0 && (
                <ul className="agent-cards-batch-list">
                  {included.map((entry) => (
                    <li key={`${entry.pane_id}\u0000${entry.agent_id}`} className="agent-cards-batch-entry is-included" data-testid="agent-cards-batch-entry">
                      <p className="agent-cards-batch-who"><strong className="agent-cards-batch-name">{rowName(entry, names)}</strong>{entry.prompt_source === "agent" && <SourceBadge source="agent" />}</p>
                      {entry.prompt
                        ? <><blockquote className="agent-card-prompt-text agent-cards-batch-prompt" data-testid="agent-cards-batch-prompt"><ExactText text={entry.prompt} /></blockquote><ExactTextNote text={entry.prompt} /></>
                        : <p className="agent-cards-batch-what">{entry.kind === "close_keep_worktree" ? "Closes this agent and keeps its worktree." : entry.kind === "close" ? "Closes this agent." : `Runs ${label} on this agent.`}</p>}
                    </li>
                  ))}
                </ul>
              )}
              {skipped.length > 0 && (
                <>
                  <h3>Skipped: {skipped.length}</h3>
                  <ul className="agent-cards-batch-list">
                    {skipped.map((entry) => (
                      <li key={`${entry.pane_id}\u0000${entry.agent_id}`} className="agent-cards-batch-entry is-skipped" data-testid="agent-cards-batch-entry">
                        <p className="agent-cards-batch-who"><strong className="agent-cards-batch-name">{rowName(entry, names)}</strong></p>
                        <p className="agent-cards-batch-reason">{displayText(entry.message ?? "it is not offered on this card", DISPLAY_LIMITS.message)}</p>
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </>
          )}
          {done && (
            <>
              <h3 role="status">{failed === 0 ? `Sent to all ${results.length}` : `${results.length - failed} of ${results.length} sent; ${failed} failed`}</h3>
              <ul className="agent-cards-batch-list">
                {results.map((entry) => (
                  <li key={`${entry.pane_id}\u0000${entry.agent_id}`} className={`agent-cards-batch-entry is-${entry.outcome === "failed" ? "failed" : "done"}`} data-testid="agent-cards-batch-entry" data-outcome={entry.outcome}>
                    <p className="agent-cards-batch-who"><strong className="agent-cards-batch-name">{rowName(entry, names)}</strong><span className="agent-cards-batch-outcome">{RESULT_WORD[entry.outcome] ?? displayText(entry.outcome, DISPLAY_LIMITS.name)}</span></p>
                    {entry.outcome === "failed" && <p className="agent-cards-batch-reason">{displayText(entry.message ?? "the deck did not act on this agent", DISPLAY_LIMITS.message)}</p>}
                  </li>
                ))}
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
