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

/** Which question an answer is for: the run, and the question within it. */
type QuestionRef = Pick<UpgradeDecisionEvent, "upgradeId" | "questionId">;

/**
 * Whether two refer to the same question. A run that asks again — what would
 * stop changed — asks a new question, so its upgrade id alone is not enough
 * (PRD #1487, Greptile 4208066960).
 */
function sameQuestion(a: QuestionRef | undefined, b: QuestionRef | undefined): boolean {
  return a !== undefined && b !== undefined && a.upgradeId === b.upgradeId && a.questionId === b.questionId;
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
 *
 * `autoStart` skips the confirmation and starts at once: the app upgrading the
 * local daemon on its own at launch, as the TUI does (issue #1636). Nothing is
 * stopped on that path without the same explicit Restart now — the daemon asks
 * whenever something is running on it.
 */
export function UpgradeDialog({ target, runtime, onClose, autoStart = false }: {
  target: UpgradeTarget;
  runtime: Pick<DeckRuntimeState, "upgradeDaemon" | "decideUpgrade">;
  onClose: () => void;
  autoStart?: boolean;
}) {
  const [state, setState] = useState<Phase>(autoStart && runtime.upgradeDaemon ? { phase: "running" } : { phase: "confirm" });
  /** The question still waiting, for the unmount answer below. */
  const pendingQuestion = useRef<QuestionRef | undefined>(undefined);
  /** The question a button's answer is on its way to, if any. */
  const answering = useRef<QuestionRef | undefined>(undefined);
  const deck = displayText(target.deckName, DISPLAY_LIMITS.name);
  const replace = target.kind === "replace";
  /* The local deck's daemon is the one on this machine, whatever the deck is called. */
  const where = target.kind === "upgrade" ? deck : "this machine";

  const send = (question: QuestionRef, choice: UpgradeChoice): Promise<void> => runtime.decideUpgrade
    ? runtime.decideUpgrade(question.upgradeId, question.questionId, choice)
    : Promise.reject(new Error("answering is not available"));

  /** For a dialog on its way out: nothing is left to show a failure on. */
  const answer = (question: QuestionRef, choice: UpgradeChoice) => {
    if (sameQuestion(pendingQuestion.current, question)) pendingQuestion.current = undefined;
    // A refusal means the question already closed (it timed out, the run
    // ended, or it was asked again); the outcome that follows says what
    // happened.
    void send(question, choice).catch(() => undefined);
  };

  /**
   * Whether the dialog is still on screen. The run outlives it — the bridge
   * keeps hearing its events until the upgrade returns — so everything the
   * run calls back into checks this first.
   */
  const mounted = useRef(true);

  // Unmounting with the question open is a Keep: the dialog went away. A
  // question asked AFTER it went is a Keep too (`start`), answered at once:
  // nothing is left to ask, and the run would otherwise wait out the crate's
  // ten-minute decision timeout holding this deck's upgrade. A question whose
  // answer is already on its way is left to that answer — a second, opposite
  // one for the same upgrade must not race it (PRD #1487, Qodo #15); if it
  // then fails, `decide` answers Keep, never Restart now.
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      const open = pendingQuestion.current;
      if (open && !sameQuestion(answering.current, open)) answer(open, "keep-current");
    };
  }, []);

  const start = () => {
    const upgrade = runtime.upgradeDaemon;
    if (!upgrade) return;
    setState({ phase: "running" });
    upgrade(target.deckId, (event) => {
      if (!mounted.current) {
        // The dialog's own listener is gone; all that is left of it answers.
        if (event.type === "decision") answer(event, "keep-current");
        return;
      }
      if (event.type === "progress") {
        setState((current) => current.phase === "running" || current.phase === "deciding" ? { ...current, stage: event.progress.stage } : current);
      } else {
        // A question asked again replaces the one on screen, even while an
        // answer to that one is still on its way.
        pendingQuestion.current = { upgradeId: event.upgradeId, questionId: event.questionId };
        setState((current) => ({ phase: "deciding", stage: current.phase === "running" || current.phase === "deciding" ? current.stage : "restarting", question: event, answering: false }));
      }
    }).then(
      (outcome) => {
        pendingQuestion.current = undefined;
        if (mounted.current) setState({ phase: "done", outcome });
      },
      (cause: unknown) => {
        pendingQuestion.current = undefined;
        if (mounted.current) setState({ phase: "error", message: cause instanceof Error ? cause.message : String(cause) });
      },
    );
  };

  /* Focus inside the dialog from the start, so Escape and Tab reach it: an
     autoStart dialog opens in its running phase, with no button to take it,
     over whatever had focus (Qodo, PR #1640). A button that autofocused keeps it. */
  const section = useRef<HTMLElement>(null);
  useEffect(() => {
    const element = section.current;
    if (element && !element.contains(document.activeElement)) element.focus();
  }, []);

  /* Started once, however often React runs the effect (StrictMode runs it twice). */
  const autoStarted = useRef(false);
  useEffect(() => {
    if (!autoStart || autoStarted.current || !runtime.upgradeDaemon) return;
    autoStarted.current = true;
    start();
  }, []);

  const decide = (choice: UpgradeChoice) => {
    if (state.phase !== "deciding" || state.answering) return;
    const question: QuestionRef = { upgradeId: state.question.upgradeId, questionId: state.question.questionId };
    // Only THIS question's screen is changed by its answer: one asked again
    // while the answer was on its way stays up, to be answered in turn.
    const asked = (current: Phase): current is Extract<Phase, { phase: "deciding" }> =>
      current.phase === "deciding" && sameQuestion(current.question, question);
    setState({ ...state, answering: true, undelivered: false });
    // The question stays open until the answer has arrived; an unmount in the
    // meantime leaves it to this answer.
    answering.current = question;
    const settled = () => {
      if (sameQuestion(answering.current, question)) answering.current = undefined;
    };
    send(question, choice).then(
      () => {
        settled();
        if (sameQuestion(pendingQuestion.current, question)) pendingQuestion.current = undefined;
        setState((current) => asked(current) ? { phase: "running", stage: current.stage } : current);
      },
      () => {
        settled();
        // Gone while it was on its way: nothing is left to retry from, so the
        // question is answered the way every other way out answers it.
        if (!mounted.current) {
          if (sameQuestion(pendingQuestion.current, question)) answer(question, "keep-current");
          return;
        }
        setState((current) => asked(current) ? { ...current, answering: false, undelivered: true } : current);
      },
    );
  };

  /** Escape, a click outside, or the dialog's own close. Never a restart. */
  const dismiss = () => {
    if (state.phase === "running") return; // nothing to answer, and it is still working
    if (state.phase === "deciding") {
      if (state.answering) return; // an answer is on its way; let it land
      answer(state.question, "keep-current");
    }
    onClose();
  };

  const title = state.phase === "confirm"
    ? (replace ? "Replace the incompatible daemon?" : `Upgrade the daemon on ${where}?`)
    : state.phase === "deciding" ? "Restart and stop these?"
      : state.phase === "done" ? outcomeView(state.outcome, deck, target.kind).title
        : state.phase === "error" ? (replace ? "Replace daemon could not start" : "Upgrade could not start")
          : replace ? "Replacing the daemon…" : target.kind === "local-upgrade" ? "Upgrading the daemon on this machine…" : `Upgrading ${deck}…`;

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={dismiss}>
      <section
        ref={section}
        tabIndex={-1}
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
        {state.phase === "deciding" && <DecisionBody question={state.question} deck={where} kind={target.kind} answering={state.answering} undelivered={state.undelivered ?? false} onDecide={decide} />}
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
    : target.kind === "local-upgrade"
      ? `The daemon on this machine runs ${offer ? displayText(offer.from, DISPLAY_LIMITS.name) : "an older version"}, and this app is ${offer ? displayText(offer.to, DISPLAY_LIMITS.name) : "newer"}. Agent Deck restarts the daemon onto the version that came with this app. If agents or orchestration roles are running on it, you are shown each one and asked before anything is stopped.`
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
        : kind === "local-upgrade"
          ? "Keep current daemon leaves them running on the old version. Press Upgrade when they have finished."
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
