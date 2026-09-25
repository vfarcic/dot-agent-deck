import { cleanupWarning } from "../lib/newAgent";

/**
 * PRD #1223 audits F6 and V7 — the roles a failed launch could not confirm are
 * stopped, shown as their own alert wherever such a failure is reported: in the
 * New agent dialog, and on the deck's global toast when the dialog is gone or
 * the failure came from the Runs launcher.
 *
 * One component rather than a sentence built per site, because the parts that
 * must survive are the same everywhere: the count and the instruction first,
 * then the names as a LIST — each clamped on its own, with the ones past the
 * cap counted rather than dropped (audit V7). Every string here has been
 * through `displayText`, so a role name carrying bidi or control characters
 * cannot reorder what is around it.
 */
export function CleanupWarning({ stops, testId, className }: { stops: readonly string[]; testId: string; className?: string }) {
  const warning = cleanupWarning(stops);
  return (
    <div className={className ? `cleanup-warning ${className}` : "cleanup-warning"} role="alert" data-testid={testId}>
      <p>{warning.summary}</p>
      <ul>
        {warning.names.map((name, index) => <li key={`${index}-${name}`}>{name}</li>)}
      </ul>
      {warning.overflow > 0 && <p data-testid={`${testId}-overflow`}>…and {warning.overflow} more</p>}
    </div>
  );
}
