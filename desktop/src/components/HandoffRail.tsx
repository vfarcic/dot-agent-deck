import { ArrowRight, CheckCircle2, CircleDashed, RefreshCw, Send, XCircle } from "lucide-react";
import type { HandoffEdge } from "../types";

const STATUS_LABEL: Record<HandoffEdge["status"], string> = {
  dispatched: "dispatched",
  delivered: "delivered · working",
  failed: "FAILED",
  done: "work-done",
};

function StatusIcon({ status }: { status: HandoffEdge["status"] }) {
  if (status === "done") return <CheckCircle2 size={13} aria-hidden="true" />;
  if (status === "failed") return <XCircle size={13} aria-hidden="true" />;
  if (status === "delivered") return <Send size={12} aria-hidden="true" />;
  return <CircleDashed size={13} aria-hidden="true" />;
}

/**
 * Live handoff chain (handoff-visibility PRD D2): one row per delegation,
 * newest first, driven entirely by the daemon's handoff events. A failed edge
 * shows its reason inline — the whole point is that a dropped handoff is
 * impossible to miss.
 */
export function HandoffRail({ handoffs }: { handoffs: HandoffEdge[] }) {
  if (!handoffs.length) return null;
  return (
    <section className="handoff-rail" aria-label="Live handoffs" data-testid="handoff-rail">
      <header>
        <span className="section-kicker">HANDOFFS</span>
        <small>Delegations live from the daemon — dispatched → delivered → work-done</small>
      </header>
      <div className="handoff-list">
        {handoffs.slice(0, 12).map((edge) => (
          <div
            key={edge.id}
            className={`handoff-edge is-${edge.status}`}
            data-testid={`handoff-${edge.id}`}
            title={edge.taskPreview ?? edge.id}
          >
            <span className="handoff-endpoint">orchestrator</span>
            <ArrowRight size={12} aria-hidden="true" />
            <strong className="handoff-endpoint">{edge.toRole}</strong>
            {edge.respawned && (
              <span className="handoff-respawn" title="Worker was respawned for this delegation">
                <RefreshCw size={10} aria-hidden="true" />
              </span>
            )}
            <em className={`handoff-status is-${edge.status}`}>
              <StatusIcon status={edge.status} /> {STATUS_LABEL[edge.status]}
            </em>
            <time>{edge.at}</time>
            {edge.status === "failed" && edge.reason && <p className="handoff-reason">{edge.reason}</p>}
            {edge.taskPreview && edge.status !== "failed" && <p className="handoff-task">{edge.taskPreview}</p>}
          </div>
        ))}
      </div>
    </section>
  );
}
