import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Activity } from "lucide-react";

/** The gap kept between the popover and the window's edges, and the voice row. */
const POPOVER_MARGIN = 12;

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
 * **Shown** is `open` (a click or voice, held by the overview) or a mouse
 * hovering over it. Hover is tracked here rather than with CSS `:hover` so a
 * close can also hide a hover preview: a second click, an Escape or a spoken
 * `close` with the pointer still over the button would otherwise leave the
 * list on screen while `aria-expanded` said it was closed (PR #2317 review).
 * After any close the hover preview stays off until the pointer leaves or
 * comes back.
 *
 * The counters stay in the DOM while hidden, so their text is still there for
 * a test to read; only showing them makes them visible.
 *
 * Dismissed the way the Host popover is (`HostMetricsButton`): a press or an
 * Escape outside it closes it, and an Escape inside it closes it and hands
 * focus back to the button. Kept inside the window the same way too: shifted
 * left by any overflow past the right edge, and limited in height to the room
 * above the voice row, scrolling past that. Measured again whenever the
 * window, the button or the list changes size.
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
  const popover = useRef<HTMLDivElement>(null);
  const popoverId = useId();
  const [hovering, setHovering] = useState(false);
  const [hoverSuppressed, setHoverSuppressed] = useState(false);
  const shown = open || (hovering && !hoverSuppressed);

  /* Every close, whichever path it took, turns the hover preview off. */
  const wasOpen = useRef(open);
  useEffect(() => {
    if (wasOpen.current && !open) setHoverSuppressed(true);
    wasOpen.current = open;
  }, [open]);

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

  const [place, setPlace] = useState<{ shift: number; maxHeight?: number }>({ shift: 0 });
  useLayoutEffect(() => {
    if (!shown) {
      setPlace({ shift: 0 });
      return;
    }
    const measure = () => {
      const element = popover.current;
      const anchor = root.current;
      if (!element || !anchor) return;
      const anchorBox = anchor.getBoundingClientRect();
      const overflow = anchorBox.left + element.getBoundingClientRect().width - (window.innerWidth - POPOVER_MARGIN);
      const shift = overflow > 0 ? Math.min(overflow, Math.max(0, anchorBox.left - POPOVER_MARGIN)) : 0;
      /* The voice row is fixed over the bottom of the window, so the room
         ends at its top where there is one. */
      const floor = document.querySelector(".voice-row")?.getBoundingClientRect().top ?? window.innerHeight;
      const top = element.getBoundingClientRect().top;
      const room = Math.floor(floor - POPOVER_MARGIN - top);
      setPlace({ shift, maxHeight: room > 0 && room < element.scrollHeight ? Math.max(room, 80) : undefined });
    };
    measure();
    window.addEventListener("resize", measure);
    /* And whenever the button or the list changes size while shown: a count
       change adds or drops an alert on the button, which moves it in the
       phone layout, and a longer number widens the list (PR #2317 review). */
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(measure);
    if (root.current) observer?.observe(root.current);
    if (popover.current) observer?.observe(popover.current);
    return () => {
      window.removeEventListener("resize", measure);
      observer?.disconnect();
    };
  }, [shown]);

  const words = countersWords(summary);
  return (
    <div
      className={`dashboard-counters${shown ? " is-shown" : ""}${open ? " is-open" : ""}`}
      ref={root}
      data-testid="dashboard-counters"
      onPointerEnter={(event) => {
        if (event.pointerType !== "mouse") return;
        setHovering(true);
        setHoverSuppressed(false);
      }}
      onPointerLeave={(event) => {
        if (event.pointerType !== "mouse") return;
        setHovering(false);
        setHoverSuppressed(false);
      }}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !shown) return;
        event.stopPropagation();
        if (open) onClose();
        else setHoverSuppressed(true);
        button.current?.focus();
      }}
    >
      <button
        ref={button}
        type="button"
        className="button secondary compact dashboard-counters-button"
        data-testid="dashboard-counters-button"
        aria-expanded={shown}
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
      <div
        ref={popover}
        className="dashboard-counters-popover"
        id={popoverId}
        role="group"
        aria-label="Counters"
        data-testid="dashboard-counters-popover"
        style={place.shift > 0 || place.maxHeight !== undefined ? { left: place.shift > 0 ? -place.shift : undefined, maxHeight: place.maxHeight } : undefined}
      >
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
