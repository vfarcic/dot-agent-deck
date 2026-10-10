import type { HostMetricsReport } from "../types";
import { formatDisk, formatGib, formatLoad, formatSampleAge, HOST_NOT_AVAILABLE, HOST_NOT_AVAILABLE_WHY, HOST_SUBTITLE, HOST_TITLE, hostRoleLabel } from "../lib/hostMetrics";
import { displayText, DISPLAY_LIMITS } from "../lib/displayText";

/**
 * PRD #1258 M4 — one deck's host on its overview card: disk per watched role,
 * load per core, memory and the sample's age, as that deck's daemon reports
 * them. The same rows and words as the TUI's host overlay.
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
          {report.metrics.disks.map((disk) => (
            <div className="host-metrics-row" key={disk.role}>
              <dt>{displayText(hostRoleLabel(disk.role), DISPLAY_LIMITS.message)}</dt>
              <dd>{formatDisk(disk.freeBytes, disk.totalBytes)}</dd>
            </div>
          ))}
          <div className="host-metrics-row">
            <dt>Load per core</dt>
            <dd>{formatLoad(report.metrics.loadPerCpu, report.metrics.cpuCount)}</dd>
          </div>
          <div className="host-metrics-row">
            <dt>Memory used</dt>
            <dd>{formatGib(report.metrics.memoryUsedBytes)}</dd>
          </div>
          <div className="host-metrics-row">
            <dt>Memory available</dt>
            <dd>{formatGib(report.metrics.memoryAvailableBytes)}</dd>
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
