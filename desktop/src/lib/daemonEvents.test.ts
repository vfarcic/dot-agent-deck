import { describe, expect, it } from "vitest";
import { applyHandoffEvent, mapDaemonEvent } from "./daemonEvents";
import type { HandoffEdge } from "../types";

const TS = "2026-08-08T21:00:00Z";

function handoffPayload(eventType: string, sessionId: string, metadata: Record<string, string>) {
  return {
    kind: "event",
    session_id: sessionId,
    agent_type: "none",
    event_type: eventType,
    timestamp: TS,
    metadata,
  };
}

describe("applyHandoffEvent", () => {
  it("walks the healthy lifecycle: dispatched → respawned → delivered → done", () => {
    let edges: HandoffEdge[] = [];
    edges = applyHandoffEvent(edges, handoffPayload("delegation_dispatched", "dlg-1", {
      to_role: "coder",
      orchestration: "dot-agent-deck",
      task_preview: "Implement the thing.",
    }));
    expect(edges).toHaveLength(1);
    expect(edges[0]).toMatchObject({ id: "dlg-1", toRole: "coder", status: "dispatched", respawned: false });

    edges = applyHandoffEvent(edges, handoffPayload("worker_respawned", "dlg-1", { to_role: "coder" }));
    expect(edges[0].respawned).toBe(true);
    expect(edges[0].status).toBe("dispatched");

    edges = applyHandoffEvent(edges, handoffPayload("delegation_delivered", "dlg-1", { to_role: "coder" }));
    expect(edges[0].status).toBe("delivered");

    edges = applyHandoffEvent(edges, handoffPayload("work_done_received", "dlg-99", { from_role: "coder", done: "false" }));
    expect(edges[0].status).toBe("done");
  });

  it("creates a failed edge even when the failure had no prior dispatch (no-pane case)", () => {
    const edges = applyHandoffEvent([], handoffPayload("delegation_failed", "dlg-2", {
      to_role: "reviewer",
      reason: "no live pane is registered for this role in the orchestration",
    }));
    expect(edges).toHaveLength(1);
    expect(edges[0]).toMatchObject({ status: "failed", toRole: "reviewer" });
    expect(edges[0].reason).toContain("no live pane");
  });

  it("marks a dispatched edge failed with the daemon's reason", () => {
    let edges = applyHandoffEvent([], handoffPayload("delegation_dispatched", "dlg-3", { to_role: "tester" }));
    edges = applyHandoffEvent(edges, handoffPayload("delegation_failed", "dlg-3", {
      to_role: "tester",
      reason: "worker respawn failed: command not found",
    }));
    expect(edges[0].status).toBe("failed");
    expect(edges[0].reason).toContain("respawn failed");
  });

  it("correlates work-done by role to the newest delivered edge only", () => {
    let edges: HandoffEdge[] = [];
    edges = applyHandoffEvent(edges, handoffPayload("delegation_dispatched", "dlg-old", { to_role: "coder" }));
    edges = applyHandoffEvent(edges, handoffPayload("delegation_delivered", "dlg-old", { to_role: "coder" }));
    edges = applyHandoffEvent(edges, handoffPayload("delegation_dispatched", "dlg-new", { to_role: "coder" }));
    edges = applyHandoffEvent(edges, handoffPayload("delegation_delivered", "dlg-new", { to_role: "coder" }));
    edges = applyHandoffEvent(edges, handoffPayload("work_done_received", "dlg-x", { from_role: "coder", done: "false" }));
    expect(edges.find((edge) => edge.id === "dlg-new")?.status).toBe("done");
    expect(edges.find((edge) => edge.id === "dlg-old")?.status).toBe("delivered");
  });

  it("returns the same reference for non-handoff payloads", () => {
    const edges: HandoffEdge[] = [];
    expect(applyHandoffEvent(edges, handoffPayload("tool_start", "sess", {}))).toBe(edges);
    expect(applyHandoffEvent(edges, { kind: "orchestration_surface" })).toBe(edges);
    expect(applyHandoffEvent(edges, null)).toBe(edges);
  });
});

describe("mapDaemonEvent handoff rows", () => {
  it("renders a failed delegation as an ERROR evidence row with the reason", () => {
    const item = mapDaemonEvent(
      handoffPayload("delegation_failed", "dlg-9", { to_role: "auditor", reason: "worker respawn failed: exec" }),
      1,
    );
    expect(item).toBeDefined();
    expect(item?.verdict).toBe("ERROR");
    expect(item?.to).toBe("auditor");
    expect(item?.summary).toContain("respawn failed");
  });

  it("renders work-done as a PASS row from the worker back to the orchestrator", () => {
    const item = mapDaemonEvent(
      handoffPayload("work_done_received", "dlg-10", { from_role: "coder", done: "true", task_preview: "Summary at .dot-agent-deck/final.md" }),
      2,
    );
    expect(item?.verdict).toBe("PASS");
    expect(item?.from).toBe("coder");
    expect(item?.to).toBe("orchestrator");
    expect(item?.summary).toContain("Run reported complete");
  });
});
