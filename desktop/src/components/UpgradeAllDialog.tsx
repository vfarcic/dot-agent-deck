import { useEffect, useRef, useState } from "react";
import { CircleArrowUp, CircleStop, ShieldAlert } from "lucide-react";

import { OVERLAY_LAYER, useInertBackground } from "../hooks/useInertBackground";
import type { DeckRuntimeState } from "../types";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { stopSetCount, stopSetLines, type UpgradeChoice, type UpgradeDecisionEvent, type UpgradeStage } from "../lib/upgrade";
import {
  UPGRADE_ALL_CHECKING,
  UPGRADE_ALL_NOTHING,
  UPGRADE_ALL_RUNNING,
  UPGRADE_ALL_RUNNING_CLOSE_BLOCKED,
  UPGRADE_ALL_STOP,
  UPGRADE_ALL_STOPPING,
  UPGRADE_ALL_TITLE,
  UPGRADE_ALL_WAITING,
  upgradeAllQuestion,
  upgradingLine,
  type DeckReport,
  type UpgradeAllPlan,
  type UpgradeAllReport,
} from "../lib/upgradeAll";

type Question = Pick<UpgradeDecisionEvent, "upgradeId" | "questionId" | "deckId" | "atStake" | "stale">;

type Phase =
  | { phase: "checking" }
  | { phase: "plan"; plan: UpgradeAllPlan }
  | { phase: "running"; plan: UpgradeAllPlan; current?: number; stage?: UpgradeStage; finished: (DeckReport | undefined)[]; question?: Question; answering: boolean; undelivered?: boolean; stopping: boolean }
  | { phase: "done"; report: UpgradeAllReport }
  | { phase: "error"; message: string; started: boolean };

/**
 * Issue #1600 — **Upgrade all**: every remote deck on the deck list checked,
 * then the older daemons upgraded one after another by the root crate's batch,
 * the one the CLI's `remote upgrade --all` and the TUI's Upgrade all daemons
 * dialog run too. The same steps and words as the TUI's (CLAUDE.md rule 22):
 * checking, the plan (Cancel or Upgrade), each deck as it goes with Stop after
 * this daemon, a busy daemon's own question (Keep current daemon or Restart
 * now), and one line per deck at the end.
 *
 * Every way out of a question that is not **Restart now** keeps the daemon
 * that asked: Keep current daemon, Escape, and the dialog going away. Escape
 * while the batch runs is Stop after this daemon, as in the TUI; the deck
 * being upgraded finishes either way.
 */
export function UpgradeAllDialog({ runtime, onClose }: {
  runtime: Pick<DeckRuntimeState, "upgradeAll" | "decideUpgrade">;
  onClose: () => void;
}) {
  const [state, setState] = useState<Phase>({ phase: "checking" });
  const mounted = useRef(true);
  /** The question still waiting, answered Keep if the dialog goes away. */
  const pending = useRef<Question | undefined>(undefined);
  const batchId = useRef<string | undefined>(undefined);

  const send = (question: Question, choice: UpgradeChoice) => runtime.decideUpgrade
    ? runtime.decideUpgrade(question.upgradeId, question.questionId, choice)
    : Promise.reject(new Error("answering is not available"));

  useEffect(() => {
    mounted.current = true;
    const upgradeAll = runtime.upgradeAll;
    if (upgradeAll) {
      upgradeAll.check().then(
        (plan) => { if (mounted.current) setState({ phase: "plan", plan }); },
        (cause: unknown) => { if (mounted.current) setState({ phase: "error", message: cause instanceof Error ? cause.message : String(cause), started: false }); },
      );
    }
    return () => {
      mounted.current = false;
      const open = pending.current;
      pending.current = undefined;
      if (open) void send(open, "keep-current").catch(() => undefined);
    };
  }, []);

  const start = () => {
    if (state.phase !== "plan" || !runtime.upgradeAll) return;
    const plan = state.plan;
    batchId.current = plan.batchId;
    setState({ phase: "running", plan, finished: plan.decks.map(() => undefined), answering: false, stopping: false });
    runtime.upgradeAll.run(plan.batchId, (event) => {
      if (!mounted.current) {
        if (event.type === "decision") void send(event, "keep-current").catch(() => undefined);
        return;
      }
      if (event.type === "decision") {
        pending.current = event;
        setState((current) => current.phase === "running" ? { ...current, question: event, answering: false, undelivered: false } : current);
        return;
      }
      setState((current) => {
        if (current.phase !== "running") return current;
        switch (event.type) {
          case "started":
            return { ...current, current: event.index, stage: undefined };
          case "progress":
            return current.current === event.index ? { ...current, stage: event.stage } : current;
          case "finished": {
            const finished = current.finished.slice();
            finished[event.index] = event.report;
            return { ...current, finished, ...(current.current === event.index ? { current: undefined, stage: undefined } : {}) };
          }
        }
      });
    }).then(
      (report) => {
        pending.current = undefined;
        if (mounted.current) setState({ phase: "done", report });
      },
      (cause: unknown) => {
        pending.current = undefined;
        if (mounted.current) setState({ phase: "error", message: cause instanceof Error ? cause.message : String(cause), started: true });
      },
    );
  };

  const stop = () => {
    if (state.phase !== "running" || state.stopping || state.question) return;
    setState({ ...state, stopping: true });
    // Refused (the batch had already ended): offer Stop again rather than
    // claim the batch is stopping.
    if (batchId.current && runtime.upgradeAll) {
      runtime.upgradeAll.stop(batchId.current).catch(() => {
        if (mounted.current) setState((current) => current.phase === "running" ? { ...current, stopping: false } : current);
      });
    }
  };

  const decide = (choice: UpgradeChoice) => {
    if (state.phase !== "running" || !state.question || state.answering) return;
    const question = state.question;
    setState({ ...state, answering: true, undelivered: false });
    send(question, choice).then(
      () => {
        if (pending.current?.questionId === question.questionId) pending.current = undefined;
        setState((current) => current.phase === "running" && current.question?.questionId === question.questionId ? { ...current, question: undefined, answering: false } : current);
      },
      () => {
        // Not delivered: the daemon may still be waiting, so the question
        // stays up to be answered again, as in the single Upgrade's dialog.
        // One that has since closed is replaced by the deck's result or a
        // newer question when they arrive.
        if (!mounted.current) return;
        setState((current) => current.phase === "running" && current.question?.questionId === question.questionId ? { ...current, answering: false, undelivered: true } : current);
      },
    );
  };

  /** A click outside, voice's "close", or the dialog's own close. Never a restart. */
  const dismiss = (): string | undefined => {
    if (state.phase === "running") {
      if (state.question && !state.answering) {
        decide("keep-current");
        return undefined;
      }
      return UPGRADE_ALL_RUNNING_CLOSE_BLOCKED;
    }
    onClose();
    return undefined;
  };

  const section = useInertBackground<HTMLElement>(true, OVERLAY_LAYER.dialog, () => dismiss());

  useEffect(() => {
    const element = section.current;
    if (element && !element.contains(document.activeElement)) element.focus();
  }, []);

  const failed = state.phase === "error" || (state.phase === "done" && state.report.failed);
  const asking = state.phase === "running" && state.question !== undefined;

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={dismiss}>
      <section
        ref={section}
        tabIndex={-1}
        className="confirm-dialog upgrade-dialog upgrade-all-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="upgrade-all-title"
        data-testid="upgrade-all-dialog"
        data-phase={state.phase}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key !== "Escape") return;
          event.stopPropagation();
          // Escape answers what changes nothing: Keep at a question, Stop
          // after this daemon while running (the TUI's Esc), Cancel otherwise.
          if (state.phase === "running" && !state.question) stop();
          else dismiss();
        }}
      >
        <div className={failed || asking ? "danger-icon" : "upgrade-icon"}>
          {asking ? <CircleStop size={20} /> : state.phase === "error" ? <ShieldAlert size={20} /> : <CircleArrowUp size={20} />}
        </div>
        <h2 id="upgrade-all-title">{UPGRADE_ALL_TITLE}</h2>
        {state.phase === "checking" && <p data-testid="upgrade-all-checking" aria-busy="true">{UPGRADE_ALL_CHECKING}</p>}
        {state.phase === "plan" && <PlanBody plan={state.plan} onCancel={onClose} onStart={start} />}
        {state.phase === "running" && <RunBody state={state} onStop={stop} onDecide={decide} />}
        {state.phase === "done" && <DoneBody report={state.report} onClose={onClose} />}
        {state.phase === "error" && (
          <>
            <p data-testid="upgrade-all-error">{displayText(state.message, DISPLAY_LIMITS.message)}</p>
            {!state.started && <p>Nothing was checked or upgraded.</p>}
            <div><button className="button primary" data-testid="upgrade-all-close" autoFocus onClick={onClose}>Close</button></div>
          </>
        )}
      </section>
    </div>
  );
}

function DeckLines({ rows }: { rows: { deck: string; line: string; status: string }[] }) {
  return (
    <ul className="upgrade-all-decks" data-testid="upgrade-all-decks">
      {rows.map((row, index) => (
        <li key={index} data-testid="upgrade-all-deck" data-status={row.status}>
          <strong>{displayText(row.deck, DISPLAY_LIMITS.name)}:</strong> {displayText(row.line, DISPLAY_LIMITS.message)}
        </li>
      ))}
    </ul>
  );
}

function PlanBody({ plan, onCancel, onStart }: { plan: UpgradeAllPlan; onCancel: () => void; onStart: () => void }) {
  return (
    <>
      <p data-testid="upgrade-all-headline"><strong>{displayText(plan.headline, DISPLAY_LIMITS.message)}</strong></p>
      <DeckLines rows={plan.decks.map((deck) => ({ deck: deck.deck, line: deck.line, status: deck.kind }))} />
      {plan.toUpgrade === 0
        ? (
          <>
            <p data-testid="upgrade-all-question"><strong>{UPGRADE_ALL_NOTHING}</strong></p>
            <div><button className="button primary" data-testid="upgrade-all-close" autoFocus onClick={onCancel}>Close</button></div>
          </>
        )
        : (
          <>
            <p data-testid="upgrade-all-question"><strong>{upgradeAllQuestion(plan.toUpgrade)}</strong></p>
            <div>
              <button className="button secondary" data-testid="upgrade-all-cancel" autoFocus onClick={onCancel}>Cancel</button>
              <button className="button primary" data-testid="upgrade-all-start" onClick={onStart}>Upgrade</button>
            </div>
          </>
        )}
    </>
  );
}

function RunBody({ state, onStop, onDecide }: {
  state: Extract<Phase, { phase: "running" }>;
  onStop: () => void;
  onDecide: (choice: UpgradeChoice) => void;
}) {
  const rows = state.plan.decks.map((deck, index) => {
    const report = state.finished[index];
    if (report) return { deck: deck.deck, line: report.line, status: report.status };
    if (deck.kind !== "upgrade") return { deck: deck.deck, line: deck.line, status: deck.kind };
    if (state.current === index) return { deck: deck.deck, line: upgradingLine(state.stage), status: "running" };
    return { deck: deck.deck, line: UPGRADE_ALL_WAITING, status: "waiting" };
  });
  const asked = state.question ? state.plan.decks.find((deck) => deck.deckId === state.question?.deckId)?.deck : undefined;
  return (
    <>
      <p data-testid="upgrade-all-headline"><strong>{state.stopping ? UPGRADE_ALL_STOPPING : UPGRADE_ALL_RUNNING}</strong></p>
      <DeckLines rows={rows} />
      {state.question
        ? (
          <div data-testid="upgrade-all-decision" aria-busy={state.answering}>
            {state.question.stale && <p>What is running changed since you were asked, so here is the list again.</p>}
            <p><strong>Restarting the daemon on {displayText(asked ?? "this deck", DISPLAY_LIMITS.name)} stops {stopSetCount(state.question.atStake)}:</strong></p>
            <ul className="upgrade-stop-list" data-testid="upgrade-all-at-stake">
              {stopSetLines(state.question.atStake).map((line, index) => <li key={index}>{displayText(line, DISPLAY_LIMITS.message)}</li>)}
            </ul>
            <p className="upgrade-hint">Keep current daemon leaves them running on the old version; the new version stays installed for its next restart.</p>
            {state.undelivered && <p role="alert" data-testid="upgrade-all-decision-error">Your answer did not reach Agent Deck, so nothing has been stopped or restarted yet. Choose again to retry.</p>}
            <div>
              <button className="button secondary" data-testid="upgrade-all-keep-current" autoFocus disabled={state.answering} onClick={() => onDecide("keep-current")}>Keep current daemon</button>
              <button className="button danger" data-testid="upgrade-all-restart-now" disabled={state.answering} onClick={() => onDecide("restart-now")}>Restart now</button>
            </div>
          </div>
        )
        : !state.stopping && (
          <div><button className="button secondary" data-testid="upgrade-all-stop" onClick={onStop}>{UPGRADE_ALL_STOP}</button></div>
        )}
    </>
  );
}

function DoneBody({ report, onClose }: { report: UpgradeAllReport; onClose: () => void }) {
  return (
    <div data-testid="upgrade-all-results" data-failed={report.failed}>
      <p data-testid="upgrade-all-headline"><strong>{displayText(report.headline, DISPLAY_LIMITS.message)}</strong></p>
      <DeckLines rows={report.decks.map((deck) => ({ deck: deck.deck, line: deck.line, status: deck.status }))} />
      <div><button className="button primary" data-testid="upgrade-all-close" autoFocus onClick={onClose}>Close</button></div>
    </div>
  );
}
