import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { WorkflowPanel } from "./ConfigurationPanels";
import { DEFAULT_PROFILES } from "../data/fixture";
import type { DaemonResolvedProject } from "../types";

const PROJECT: DaemonResolvedProject = {
  path: "/home/dev/code/deck",
  displayPath: "/home/dev/code/deck",
  displayName: "deck",
  orchestrations: [{ name: "loop", displayName: "loop", default: true, roles: [{ name: "orchestrator", displayName: "orchestrator", start: true }] }],
  configRevision: "revision-1",
};

function panel(deckId: string, liveTitles: string[]) {
  return (
    <WorkflowPanel
      open
      profiles={DEFAULT_PROFILES}
      order={[]}
      mode="live"
      project={PROJECT}
      onChooseProject={vi.fn()}
      onClose={vi.fn()}
      onToggle={vi.fn()}
      onMove={vi.fn()}
      onLaunch={vi.fn()}
      deckId={deckId}
      liveTitles={liveTitles}
    />
  );
}

describe("WorkflowPanel run name", () => {
  /**
   * PR #1333 review. Scenario: the sheet is open on deck A, which runs
   * `deck-orchestrator-1`, so it suggests `-2`. A fleet tick on the SAME deck
   * that adds `-2` leaves the suggestion alone — the TUI suggests from one
   * snapshot, and the collision check covers the rest. Switching to deck B,
   * which runs nothing, recomputes the untouched suggestion against B's titles.
   */
  it("recomputes an untouched suggestion when the selected deck changes, not on every fleet tick", () => {
    const { rerender } = render(panel("deck-a", ["deck-orchestrator-1"]));
    expect(screen.getByLabelText("Run name")).toHaveValue("deck-orchestrator-2");

    rerender(panel("deck-a", ["deck-orchestrator-1", "deck-orchestrator-2"]));
    expect(screen.getByLabelText("Run name")).toHaveValue("deck-orchestrator-2");
    expect(screen.getByTestId("workflow-title-taken")).toBeVisible();

    rerender(panel("deck-b", []));
    expect(screen.getByLabelText("Run name")).toHaveValue("deck-orchestrator-1");
    expect(screen.queryByTestId("workflow-title-taken")).toBeNull();
  });
});
