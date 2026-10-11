import { useEffect, useId, useRef, useState } from "react";
import { Gauge } from "lucide-react";
import type { HostMetricsReport } from "../types";
import { formatDisk, formatGib, formatLoad, formatSampleAge, HOST_BUTTON_TEXT, HOST_NOT_AVAILABLE, HOST_NOT_AVAILABLE_WHY, HOST_SUBTITLE, HOST_TITLE, hostLevel, hostLevelWord, hostRoleLabel, hostStateWords, worstHostLevel } from "../lib/hostMetrics";
import { displayText, DISPLAY_LIMITS, domIdentity } from "../lib/displayText";

/**
 * PRD #1258 — one value as the daemon judged it: coloured by its level, and
 * followed by the level's word when it is not `ok`, so the colour is never the
 * only signal. A value with no level (an older daemon, or an absent number) is
 * written plain.
 */
function JudgedValue({ value, level }: { value: string; level: unknown }) {
  const known = hostLevel(level);
  const word = hostLevelWord(known);
  return (
    <dd className={known ? `host-level-${known}` : undefined} data-level={known ?? ""}>
      {word ? `${value} ${word}` : value}
    </dd>
  );
}

/**
 * PRD #1258 — one deck's host: disk per watched role, load per core, memory and
 * the sample's age, as that deck's daemon reports them, each coloured by the
 * level the daemon gave it. The same rows and words as the TUI's host overlay.
 * Shown in the popover that the deck header's host button opens.
 */
export function HostMetricsPanel({ report }: { report: HostMetricsReport }) {
  return (
    <section className="host-metrics" data-testid="host-metrics" aria-label={HOST_TITLE}>
      <header className="host-metrics-header">
        <strong>{HOST_TITLE}</strong>
        <span className="host-metrics-subtitle">{HOST_SUBTITLE}</span>
      </header>
      {report.status === "not-available" ? (
        <p className="host-metrics-unavailable">{HOST_NOT_AVAILABLE} {HOST_NOT_AVAILABLE_WHY}</p>
      ) : (
        <dl className="host-metrics-rows">
          {report.metrics.disks.map((disk, index) => (
            // The role is the daemon's text: the key gets its bounded copy, and
            // the index keeps two equal roles from sharing one.
            <div className="host-metrics-row" key={`${index}:${domIdentity(disk.role)}`}>
              <dt>{displayText(hostRoleLabel(disk.role), DISPLAY_LIMITS.message)}</dt>
              <JudgedValue value={formatDisk(disk.freeBytes, disk.totalBytes)} level={disk.level} />
            </div>
          ))}
          <div className="host-metrics-row">
            <dt>Load per core</dt>
            <JudgedValue value={formatLoad(report.metrics.loadPerCpu, report.metrics.cpuCount)} level={report.metrics.loadLevel} />
          </div>
          <div className="host-metrics-row">
            <dt>Memory used</dt>
            <dd>{formatGib(report.metrics.memoryUsedBytes)}</dd>
          </div>
          <div className="host-metrics-row">
            <dt>Memory available</dt>
            <JudgedValue value={formatGib(report.metrics.memoryAvailableBytes)} level={report.metrics.memoryLevel} />
          </div>
          <div className="host-metrics-row">
            <dt>Sample age</dt>
            <dd>{formatSampleAge(report.metrics.sampleAgeMs)}</dd>
          </div>
        </dl>
      )}
    </section>
  );
}

/**
 * PRD #1258 — the deck header's host button, beside the deck's name. It is
 * coloured by the worst level among the deck's readings (neutral when none
 * carries one), names that state in words in its accessible name and tooltip,
 * and opens {@link HostMetricsPanel} in a popover that Escape and a press
 * anywhere else close. A deck whose daemon does not report its host opens the
 * same popover saying so.
 */
export function HostMetricsButton({ report, deckLabel }: { report: HostMetricsReport; deckLabel: string }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const popoverId = useId();
  const level = worstHostLevel(report);
  const state = hostStateWords(report);
  const word = level === "warn" || level === "critical" ? state : undefined;

  /*
    Dismissed the way the dashboard's Filter menu is: a press anywhere outside
    this root, bound only while open; and Escape from anywhere, which also hands
    focus back to the button so a keyboard user is not left on the page body.
  */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.stopPropagation();
      setOpen(false);
      button.current?.focus();
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", dismissOnEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", dismissOnEscape, true);
    };
  }, [open]);

  return (
    <div className="host-metrics-anchor" ref={root}>
      <button
        ref={button}
        type="button"
        className={`button secondary compact host-metrics-button host-button-${level ?? "none"}`}
        data-testid="host-metrics-button"
        data-level={level ?? ""}
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-controls={open ? popoverId : undefined}
        aria-label={`${HOST_TITLE}, ${deckLabel}${state ? `: ${state}` : ""}`}
        title={`${HOST_TITLE}${state ? `: ${state}` : ""}`}
        onClick={() => setOpen((wasOpen) => !wasOpen)}
      >
        <Gauge size={13} aria-hidden="true" />
        <span>{word ? `${HOST_BUTTON_TEXT}: ${word}` : HOST_BUTTON_TEXT}</span>
      </button>
      {open && (
        <div className="host-metrics-popover" id={popoverId} role="dialog" aria-label={`${HOST_TITLE}, ${deckLabel}`} data-testid="host-metrics-popover">
          <HostMetricsPanel report={report} />
        </div>
      )}
    </div>
  );
}
