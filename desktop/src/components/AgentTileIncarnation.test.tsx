import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { createFixtureStartedAgent } from "../data/fixture";
import { AgentTile } from "./AgentTile";

vi.mock("./TerminalViewport", () => ({ TerminalViewport: () => <pre>synthetic terminal</pre> }));

/// Scenario: Rename the card's agent and then open another draft while the daemon restarts with a same-id agent. The first rename carries the listed incarnation; the stale draft disappears without renaming the successor, whose own rename carries its new incarnation.
it("binds a rename to the listed incarnation and discards a draft on restart", () => {
  const onRename = vi.fn(async () => {});
  const original = { ...createFixtureStartedAgent({ id: "7", daemonId: "deck-1666", displayName: "Original", command: "cat", cwd: "/project" }), incarnation: "process-a" };
  const card = (agent = original) => <AgentTile agent={agent} presentation="tile" showDetails={false} mode="live"
    selected={false} tab="terminal" evidence={[]} onSelect={vi.fn()} onTabChange={vi.fn()}
    onTerminalInput={vi.fn(async () => {})} onTerminalResize={vi.fn(async () => {})}
    onEvidenceSelect={vi.fn()} onRename={onRename} />;
  const { rerender } = render(card());
  fireEvent.click(screen.getByRole("button", { name: /^Rename / }));
  fireEvent.change(screen.getByRole("textbox", { name: /^Rename / }), { target: { value: "Renamed" } });
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
  expect(onRename).toHaveBeenCalledExactlyOnceWith("7", "Renamed", "process-a");

  fireEvent.click(screen.getByRole("button", { name: /^Rename / }));
  fireEvent.change(screen.getByRole("textbox"), { target: { value: "Stale draft" } });
  rerender(card({ ...original, displayName: "Successor", incarnation: "process-b" }));
  expect(screen.queryByRole("textbox", { name: /^Rename / })).toBeNull();
  expect(onRename).toHaveBeenCalledTimes(1);
  fireEvent.click(screen.getByRole("button", { name: /^Rename / }));
  fireEvent.change(screen.getByRole("textbox"), { target: { value: "Fresh name" } });
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
  expect(onRename).toHaveBeenLastCalledWith("7", "Fresh name", "process-b");
});
