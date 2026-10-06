import { useEffect, useRef, useState } from "react";
import { Check, CircleArrowUp, CircleStop, LoaderCircle, RefreshCw, ShieldAlert } from "lucide-react";

import type { DeckRuntimeState } from "../types";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import {
  outcomeView,
  stageLabel,
  stopSetCount,
  stopSetLines,
  UPGRADE_STAGES,
  type UpgradeChoice,
  type UpgradeDecisionEvent,
  type UpgradeKind,
  type UpgradeOffer,
  type UpgradeOutcome,
  type UpgradeStage,
} from "../lib/upgrade";

/** What the dialog upgrades: one deck, named the way the screen names it. */
export interface UpgradeTarget {
  deckId: string;
  /** The deck's name as the screen shows it. */
  deckName: string;
  kind: UpgradeKind;
  /** The crate's offer, for the versions the confirmation names. Absent for Replace daemon. */
  offer?: UpgradeOffer;
}

type Phase =
  | { phase: "confirm" }
  | { phase: "running"; stage?: UpgradeStage }
  | { phase: "deciding"; stage?: UpgradeStage; question: UpgradeDecisionEvent; answering: boolean; undelivered?: boolean }
  | { phase: "done"; outcome: UpgradeOutcome }
  | { phase: "error"; message: string };

/**
 * PRD #1487 M5 — Upgrade (a remote deck) and Replace daemon (the local one), as
 * one dialog over the one shared procedure: confirm, then the stages as they
 * run, then — only when the daemon has agents or orchestration roles that a
 * restart would stop — the question naming every one of them, then the
 * outcome in plain words.
 *
 * Every way out of the question that is not **Restart now** keeps the current
 * daemon: Keep current daemon, Escape, a click outside, and the dialog going
 * away. Nothing is stopped without an explicit yes.
 *
 * An answer chosen with the buttons stays on screen until it has been
 * delivered: the upgrade waits for it, so one that did not arrive is said so,
 * with the question still there to answer again.
 */
export function UpgradeDialog({ target, runtime, onClose }: {
  target: UpgradeTarget;
  runtime: Pick<DeckRuntimeState, "upgradeDaemon" | "decideUpgrade">;
  onClose: () => void;
}) {
  const [state, setState] = useState<Phase>({ phase: "confirm" });
  /** The question still waiting, for the unmount answer below. */
  const pendingQuestion = useRef<string | undefined>(undefined);
  const deck = displayText(target.deckName, DISPLAY_LIMITS.name);
  const replace = target.kind === "replace";

  const send = (upgradeId: string, choice: UpgradeChoice): Promise<void> => runtime.decideUpgrade
    ? runtime.decideUpgrade(upgradeId, choice)
    : Promise.reject(new Error("answering is not available"));

  /** For a dialog on its way out: nothing is left to show a failure on. */
  const answer = (upgradeId: string, choice: UpgradeChoice) => {
    if (pendingQuestion.current === upgradeId) pendingQuestion.current = undefined;
    // A refusal means the question already closed (it timed out, or the run
    // ended); the outcome that follows says what happened.
    void send(upgradeId, choice).catch(() => undefined);
  };

  // Unmounting with the question open is a Keep: the dialog went away.
  useEffect(() => () => {
    const open = pendingQuestion.current;
    if (open) answer(open, "keep-current");
  }, []);

  const start = () => {
    const upgrade = runtime.upgradeDaemon;
    if (!upgrade) return;
    setState({ phase: "running" });
    upgrade(target.deckId, (event) => {
      if (event.type === "progress") {
        setState((current) => current.phase === "running" || current.phase === "deciding" ? { ...current, stage: event.progress.stage } : current);
      } else {
        pendingQuestion.current = event.upgradeId;
        setState((current) => ({ phase: "deciding", stage: current.phase === "running" || current.phase === "deciding" ? current.stage : "restarting", question: event, answering: false }));
      }
    }).then(
      (outcome) => {
        pendingQuestion.current = undefined;
        setState({ phase: "done", outcome });
      },
      (cause: unknown) => {
        pendingQuestion.current = undefined;
        setState({ phase: "error", message: cause instanceof Error ? cause.message : String(cause) });
      },
    );
  };

  const decide = (choice: UpgradeChoice) => {
    if (state.phase !== "deciding" || state.answering) return;
    const { upgradeId } = state.question;
    const asked = (current: Phase): current is Extract<Phase, { phase: "deciding" }> =>
      current.phase === "deciding" && current.question.upgradeId === upgradeId;
    setState({ ...state, answering: true, undelivered: false });
    // The question stays until the answer has arrived; until then it is still
    // the one an unmount answers.
    send(upgradeId, choice).then(
      () => {
        if (pendingQuestion.current === upgradeId) pendingQuestion.current = undefined;
        setState((current) => asked(current) ? { phase: "running", stage: current.stage } : current);
      },
      () => setState((current) => asked(current) ? { ...current, answering: false, undelivered: true } : current),
    );
  };

  /** Escape, a click outside, or the dialog's own close. Never a restart. */
  const dismiss = () => {
    if (state.phase === "running") return; // nothing to answer, and it is still working
    if (state.phase === "deciding") {
      if (state.answering) return; // an answer is on its way; let it land
      answer(state.question.upgradeId, "keep-current");
    }
    onClose();
  };

  const title = state.phase === "confirm"
    ? (replace ? "Replace the incompatible daemon?" : `Upgrade the daemon on ${deck}?`)
    : state.phase === "deciding" ? "Restart and stop these?"
      : state.phase === "done" ? outcomeView(state.outcome, deck, target.kind).title
        : state.phase === "error" ? (replace ? "Replace daemon could not start" : "Upgrade could not start")
          : replace ? "Replacing the daemon…" : `Upgrading ${deck}…`;

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={dismiss}>
      <section
        className="confirm-dialog upgrade-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="upgrade-title"
        data-testid="upgrade-dialog"
        data-phase={state.phase}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            dismiss();
          }
        }}
      >
        <div className={state.phase === "done" && outcomeView(state.outcome, deck, target.kind).tone === "failure" || state.phase === "error" || state.phase === "deciding" ? "danger-icon" : "upgrade-icon"}>
          {state.phase === "deciding" ? <CircleStop size={20} /> : state.phase === "error" ? <ShieldAlert size={20} /> : replace ? <RefreshCw size={20} /> : <CircleArrowUp size={20} />}
        </div>
        <h2 id="upgrade-title">{title}</h2>
        {state.phase === "confirm" && <ConfirmBody target={target} deck={deck} onCancel={onClose} onStart={start} available={Boolean(runtime.upgradeDaemon)} />}
        {(state.phase === "running" || state.phase === "deciding") && <StageList stage={state.stage} kind={target.kind} />}
        {state.phase === "deciding" && <DecisionBody question={state.question} deck={replace ? "this machine" : deck} kind={target.kind} answering={state.answering} undelivered={state.undelivered ?? false} onDecide={decide} />}
        {state.phase === "done" && <OutcomeBody outcome={state.outcome} deck={deck} kind={target.kind} onClose={onClose} />}
        {state.phase === "error" && (
          <>
            <p data-testid="upgrade-error">{displayText(state.message, DISPLAY_LIMITS.message)}</p>
            <p>Nothing was changed.</p>
            <div><button className="button primary" data-testid="upgrade-close" autoFocus onClick={onClose}>Close</button></div>
          </>
        )}
      </section>
    </div>
  );
}

function ConfirmBody({ target, deck, available, onCancel, onStart }: { target: UpgradeTarget; deck: string; available: boolean; onCancel: () => void; onStart: () => void }) {
  const offer = target.offer?.kind === "offered" ? target.offer : undefined;
  const body = target.kind === "replace"
    ? "Agent Deck stops the daemon running on this machine and starts the one that came with this app. If agents or orchestration roles are running on it, you are shown each one and asked before anything is stopped."
    : `This installs ${offer ? displayText(offer.to, DISPLAY_LIMITS.name) : "this app's version"} on ${deck}${offer ? ` (its daemon runs ${displayText(offer.from, DISPLAY_LIMITS.name)} now)` : ""} and restarts the daemon onto it. If agents or orchestration roles are running there, you are shown each one and asked before anything is stopped.`;
  return (
    <>
      <p data-testid="upgrade-confirm-body">{body}</p>
      <div>
        <button className="button secondary" data-testid="upgrade-cancel" onClick={onCancel}>Cancel</button>
        <button className="button primary" data-testid="upgrade-start" autoFocus disabled={!available} onClick={onStart}>{target.kind === "replace" ? "Replace daemon" : "Upgrade"}</button>
      </div>
    </>
  );
}

function StageList({ stage, kind }: { stage?: UpgradeStage; kind: UpgradeKind }) {
  const current = stage ? UPGRADE_STAGES.indexOf(stage) : -1;
  return (
    <ol className="upgrade-stages" data-testid="upgrade-stages" aria-label="Progress">
      {UPGRADE_STAGES.map((name, index) => {
        const status = index < current ? "done" : index === current ? "active" : "pending";
        return (
          <li key={name} data-testid={`upgrade-stage-${name}`} data-state={status} aria-current={status === "active" ? "step" : undefined}>
            {status === "done" ? <Check size={14} /> : status === "active" ? <LoaderCircle className="spin" size={14} /> : <span className="upgrade-stage-dot" aria-hidden="true" />}
            <span>{stageLabel(name, kind)}</span>
          </li>
        );
      })}
    </ol>
  );
}

function DecisionBody({ question, deck, kind, answering, undelivered, onDecide }: { question: UpgradeDecisionEvent; deck: string; kind: UpgradeKind; answering: boolean; undelivered: boolean; onDecide: (choice: UpgradeChoice) => void }) {
  return (
    <div data-testid="upgrade-decision" aria-busy={answering}>
      {question.stale && <p data-testid="upgrade-decision-stale">What is running changed since you were asked, so here is the list again.</p>}
      <p>Restarting the daemon on {deck} stops {stopSetCount(question.atStake)}:</p>
      <StopList lines={stopSetLines(question.atStake)} testId="upgrade-at-stake" />
      <p className="upgrade-hint">{kind === "replace"
        ? "Keep current daemon leaves them running on the daemon you have now."
        : "Keep current daemon leaves them running on the old version; the new version stays installed for its next restart."}</p>
      {undelivered && <p role="alert" data-testid="upgrade-decision-error">Your answer did not reach Agent Deck, so nothing has been stopped or restarted yet. Choose again to retry.</p>}
      <div>
        <button className="button secondary" data-testid="upgrade-keep-current" autoFocus onClick={() => onDecide("keep-current")}>Keep current daemon</button>
        <button className="button danger" data-testid="upgrade-restart-now" onClick={() => onDecide("restart-now")}>Restart now</button>
      </div>
    </div>
  );
}

function OutcomeBody({ outcome, deck, kind, onClose }: { outcome: UpgradeOutcome; deck: string; kind: UpgradeKind; onClose: () => void }) {
  const view = outcomeView(outcome, deck, kind);
  return (
    <div data-testid="upgrade-outcome" data-outcome={outcome.outcome} data-tone={view.tone}>
      {view.body.map((sentence, index) => <p key={index}>{displayText(sentence, DISPLAY_LIMITS.message)}</p>)}
      {view.list && <StopList lines={view.list} testId="upgrade-outcome-list" />}
      <div><button className="button primary" data-testid="upgrade-close" autoFocus onClick={onClose}>Close</button></div>
    </div>
  );
}

function StopList({ lines, testId }: { lines: string[]; testId: string }) {
  return (
    <ul className="upgrade-stop-list" data-testid={testId}>
      {lines.map((line, index) => <li key={index}>{displayText(line, DISPLAY_LIMITS.message)}</li>)}
    </ul>
  );
}
