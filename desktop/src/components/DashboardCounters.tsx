import { useEffect, useId, useRef, type ReactNode } from "react";
import { Activity } from "lucide-react";

/**
 * The agent dashboard's counters, folded into one button (issue #2312
 * follow-up).
 *
 * The row of AGENTS, RUNNING, NEEDS INPUT … DAEMONS instruments took most of
 * the top bar at the type scale's sizes, and every daemon's own header already
 * counts its agents. So the top bar keeps a button that reads the total and
 * names only what someone has to act on — agents waiting for input, agents
 * that failed, daemons that are not answering — and the full set of counters
 * opens beneath it: on hover, on a click, or by voice (`show_dashboard_counters`).
 *
 * The counters stay in the DOM while the popover is closed (hidden by CSS),
 * so their text is still there for a test or the voice layer to read; only
 * opening it shows them.
 *
 * Dismissed the way the Host popover is (`HostMetricsButton`): a press or an
 * Escape outside it closes it, and an Escape inside it closes it and hands
 * focus back to the button.
 */
export function DashboardCounters({ summary, open, onOpen, onClose, children }: {
  summary: DashboardCountersSummary;
  open: boolean;
  onOpen: () => void;
  onClose: () => void;
  children: ReactNode;
}) {
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const popoverId = useId();
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      close.current();
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      dismiss(event);
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", dismissOnEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", dismissOnEscape, true);
    };
  }, [open]);

  const words = countersWords(summary);
  return (
    <div
      className={`dashboard-counters${open ? " is-open" : ""}`}
      ref={root}
      data-testid="dashboard-counters"
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !open) return;
        event.stopPropagation();
        onClose();
        button.current?.focus();
      }}
    >
      <button
        ref={button}
        type="button"
        className="button secondary compact dashboard-counters-button"
        data-testid="dashboard-counters-button"
        aria-expanded={open}
        aria-controls={popoverId}
        aria-label={`Counters: ${words.label}`}
        title="Counters"
        onClick={() => (open ? onClose() : onOpen())}
      >
        <Activity size={14} aria-hidden="true" />
        <span className="dashboard-counters-total">{words.totalCount}<span className="dashboard-counters-word"> {words.totalWord}</span></span>
        {words.alerts.map((alert) => (
          <span key={alert.text} className={`dashboard-counters-alert ${alert.className}`} title={alert.text}>
            {alert.count}<span className="dashboard-counters-word"> {alert.word}</span>
          </span>
        ))}
      </button>
      <div className="dashboard-counters-popover" id={popoverId} role="group" aria-label="Counters" data-testid="dashboard-counters-popover">
        {children}
      </div>
    </div>
  );
}

/** What the button summarises. `known: false` when no daemon is answering. */
export interface DashboardCountersSummary {
  known: boolean;
  agents: number;
  needsInput: number;
  failed: number;
  decksUp: number;
  decksTotal: number;
}

/**
 * The button's words: the total, then one alert per thing to act on, and the
 * whole as one sentence for its accessible name. Each is a count and a word,
 * because a narrow window shows the counts alone.
 */
export function countersWords(summary: DashboardCountersSummary): { total: string; totalCount: string; totalWord: string; alerts: CountersAlert[]; label: string } {
  const totalCount = summary.known ? String(summary.agents) : "—";
  const totalWord = summary.known && summary.agents === 1 ? "agent" : "agents";
  const alerts: CountersAlert[] = [];
  const alert = (count: string, word: string, className: string) => alerts.push({ count, word, text: `${count} ${word}`, className });
  if (summary.known && summary.needsInput > 0) alert(String(summary.needsInput), "needs input", "count-needs-input");
  if (summary.known && summary.failed > 0) alert(String(summary.failed), "failed", "count-failed");
  if (summary.decksUp < summary.decksTotal) alert(`${summary.decksUp}/${summary.decksTotal}`, "daemons", "count-failed");
  const total = `${totalCount} ${totalWord}`;
  return { total, totalCount, totalWord, alerts, label: [total, ...alerts.map((entry) => entry.text)].join(", ") };
}

/** One thing on the button someone has to act on. */
export interface CountersAlert {
  count: string;
  word: string;
  text: string;
  className: string;
}
