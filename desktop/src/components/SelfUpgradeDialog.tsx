import { useState } from "react";
import { CircleArrowUp, Copy, LoaderCircle, RotateCcw } from "lucide-react";

import { writeClipboardText } from "../lib/clipboard";
import { DISPLAY_LIMITS, displayText, sanitizeText } from "../lib/displayText";
import {
  nextOffer,
  plansOf,
  type SelfCopy,
  type SelfUpgradeApi,
  type SelfUpgradeCheck,
  type SelfUpgradeLine,
  type SelfUpgradePlan,
  type SelfUpgradeResult,
} from "../lib/selfUpgrade";

/**
 * Issue #1635 — upgrading this machine's copies of Agent Deck: the app, and the
 * `dot-agent-deck` CLI when one is installed beside it.
 *
 * It shows each copy's plan in the crate's own words, then offers the copies
 * that can be upgraded from here one at a time, the app first, each behind its
 * own Upgrade. Nothing is upgraded until Upgrade is pressed: Cancel, Escape
 * and a click outside before then change nothing. A copy that cannot be
 * upgraded from here (Nix, a source build, an app folder you cannot write)
 * shows what to do instead, with any command copyable, and only Close.
 *
 * Relaunch is offered only once the app itself was replaced (the `.dmg` swap),
 * and the app restarts only when it is pressed.
 */
export function SelfUpgradeDialog({ check, api, onClose, copyText = writeClipboardText }: {
  check: SelfUpgradeCheck;
  api: SelfUpgradeApi;
  onClose: () => void;
  /** Where Copy writes; the system clipboard in the app. */
  copyText?: (text: string) => Promise<void>;
}) {
  const [done, setDone] = useState<SelfCopy[]>([]);
  const [results, setResults] = useState<Partial<Record<SelfCopy, SelfUpgradeResult>>>({});
  const [running, setRunning] = useState<SelfCopy>();
  const [relaunchError, setRelaunchError] = useState<string>();

  const offer = running ? undefined : nextOffer(check, done);
  const phase = running ? "running" : offer ? "confirm" : "done";
  const relaunch = Object.values(results).some((result) => result?.relaunch);
  const anyRun = Object.keys(results).length > 0;

  const upgrade = (plan: SelfUpgradePlan) => {
    setRunning(plan.copy);
    api.run(plan.copy).then(
      (result) => setResults((current) => ({ ...current, [plan.copy]: result })),
      (cause: unknown) => setResults((current) => ({ ...current, [plan.copy]: { copy: plan.copy, ok: false, relaunch: false, lines: [{ text: String(cause instanceof Error ? cause.message : cause), command: false }] } })),
    ).finally(() => {
      setDone((current) => [...current, plan.copy]);
      setRunning(undefined);
    });
  };

  /* Cancel before anything ran closes the dialog having done nothing; after an
     upgrade it passes on this copy and moves to the next, or to the end. */
  const cancel = (plan: SelfUpgradePlan) => (anyRun ? setDone((current) => [...current, plan.copy]) : onClose());
  const dismiss = () => {
    if (!running) onClose();
  };
  const doRelaunch = () => {
    setRelaunchError(undefined);
    api.relaunch().catch((cause: unknown) => setRelaunchError(String(cause instanceof Error ? cause.message : cause)));
  };

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={dismiss}>
      <section
        className="confirm-dialog upgrade-dialog self-upgrade-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="self-upgrade-title"
        data-testid="self-upgrade-dialog"
        data-phase={phase}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.stopPropagation();
            dismiss();
          }
        }}
      >
        <div className="upgrade-icon"><CircleArrowUp size={20} /></div>
        <h2 id="self-upgrade-title">Upgrade to v{displayText(check.latest, DISPLAY_LIMITS.name)}</h2>
        {plansOf(check).map((plan) => (
          <PlanSection key={plan.copy} plan={plan} result={results[plan.copy]} running={running === plan.copy} copyText={copyText} />
        ))}
        {phase === "confirm" && offer && (
          <div className="self-upgrade-actions" data-testid="self-upgrade-confirm" data-copy={offer.copy}>
            <p className="self-upgrade-question" data-testid="self-upgrade-question">{displayText(offer.confirmQuestion ?? "", DISPLAY_LIMITS.message)}</p>
            <div>
              <button className="button secondary" data-testid="self-upgrade-cancel" onClick={() => cancel(offer)}>Cancel</button>
              <button className="button primary" data-testid="self-upgrade-start" autoFocus onClick={() => upgrade(offer)}>Upgrade</button>
            </div>
          </div>
        )}
        {phase === "running" && (
          <div className="self-upgrade-actions"><div><button className="button primary" disabled>Upgrading…</button></div></div>
        )}
        {phase === "done" && (
          <div className="self-upgrade-actions" data-testid="self-upgrade-done">
            {relaunchError && <p role="alert" data-testid="self-upgrade-relaunch-error">{displayText(relaunchError, DISPLAY_LIMITS.detail)}</p>}
            <div>
              <button className={relaunch ? "button secondary" : "button primary"} data-testid="self-upgrade-close" autoFocus={!relaunch} onClick={onClose}>Close</button>
              {relaunch && <button className="button primary" data-testid="self-upgrade-relaunch" autoFocus onClick={doRelaunch}><RotateCcw size={14} /><span>Relaunch</span></button>}
            </div>
          </div>
        )}
      </section>
    </div>
  );
}

function PlanSection({ plan, result, running, copyText }: { plan: SelfUpgradePlan; result?: SelfUpgradeResult; running: boolean; copyText: (text: string) => Promise<void> }) {
  return (
    <section className="self-upgrade-plan" data-testid={`self-upgrade-plan-${plan.copy}`} data-action={plan.action} aria-label={plan.label}>
      <h3>{displayText(plan.headline, DISPLAY_LIMITS.message)}</h3>
      {/* The first line is the headline, shown above. */}
      <Lines lines={plan.lines.slice(1)} copyText={copyText} testId={`self-upgrade-lines-${plan.copy}`} />
      {running && <p className="self-upgrade-running" data-testid={`self-upgrade-running-${plan.copy}`} role="status"><LoaderCircle className="spin" size={13} /><span>Upgrading…</span></p>}
      {result && (
        <div className="self-upgrade-result" data-testid={`self-upgrade-result-${plan.copy}`} data-ok={result.ok} role={result.ok ? "status" : "alert"}>
          <Lines lines={result.lines} copyText={copyText} />
        </div>
      )}
    </section>
  );
}

function Lines({ lines, copyText, testId }: { lines: SelfUpgradeLine[]; copyText: (text: string) => Promise<void>; testId?: string }) {
  return (
    <div className="self-upgrade-lines" data-testid={testId}>
      {lines.map((line, index) => line.command
        ? (
          <div className="self-upgrade-command" key={index}>
            <code>{displayText(line.text, DISPLAY_LIMITS.detail)}</code>
            <button type="button" aria-label="Copy command" title="Copy command" data-testid="self-upgrade-copy" onClick={() => void copyText(sanitizeText(line.text)).catch(() => undefined)}><Copy size={12} /></button>
          </div>
        )
        : <p key={index}>{displayText(line.text, DISPLAY_LIMITS.detail)}</p>)}
    </div>
  );
}
