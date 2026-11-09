import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState, DeckSnapshot, KeptWorktreeEntry } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/**
 * Issue #1674 — the desktop's post-close report: every dispatched worktree a
 * close kept on disk becomes one notice in the corner, naming the directory
 * in the TUI's status-line words (CLAUDE.md rule 22), held until dismissed so
 * a later sentence cannot replace the path the user may still need.
 */
function runtimeOf(deck: DeckSnapshot, keptWorktrees: KeptWorktreeEntry[], dismissWorktreeKept: (id: number) => void): DeckRuntimeState {
  const settings: DesktopSettingsDto = { ...structuredClone(DEFAULT_DESKTOP_SETTINGS), endpoints: { remote: [], selection: LOCAL_ENDPOINT_SELECTION } };
  return {
    mode: "live",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: structuredClone(settings), path: "/tmp/desktop.toml" })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => structuredClone(next)),
    snapshot: deck,
    fleet: [deck],
    keptWorktrees,
    dismissWorktreeKept,
  } as unknown as DeckRuntimeState;
}

describe("the kept-worktree toast (issue #1674)", () => {
  beforeEach(() => window.localStorage.clear());

  /**
   * Scenario: closes on two decks each kept a dispatched worktree — one with
   * uncommitted work confirmed, one the daemon could only keep conditionally.
   * One notice appears per kept worktree, each with the TUI's sentence naming
   * its directory, and dismissing one leaves the other on screen.
   */
  it("holds one notice per kept worktree, dismissed separately", () => {
    const kept: KeptWorktreeEntry[] = [
      { id: 1, path: "/home/dev/code/deck-dispatch-issue-1674", confirmedDirty: true },
      { id: 2, path: "/home/dev/code/deck-dispatch-another", confirmedDirty: false },
    ];
    const dismiss = vi.fn((id: number) => { kept.splice(kept.findIndex((entry) => entry.id === id), 1); });
    const deck = createFixtureSnapshot("connected");
    const { rerender } = render(<DeckShell runtime={runtimeOf(deck, [...kept], dismiss)} initialView={{ kind: "overview" }} />);

    const notices = screen.getAllByTestId("worktree-kept-toast");
    expect(notices).toHaveLength(2);
    expect(notices[0]).toHaveTextContent("Uncommitted work here is KEPT, not deleted: /home/dev/code/deck-dispatch-issue-1674.");
    expect(notices[1]).toHaveTextContent("Uncommitted work here, if any, is KEPT: /home/dev/code/deck-dispatch-another.");

    fireEvent.click(within(notices[0]).getByRole("button", { name: "Dismiss kept worktree notice" }));
    expect(dismiss).toHaveBeenCalledWith(1);
    rerender(<DeckShell runtime={runtimeOf(deck, [...kept], dismiss)} initialView={{ kind: "overview" }} />);
    expect(screen.getAllByTestId("worktree-kept-toast")).toHaveLength(1);
    expect(screen.getByTestId("worktree-kept-toast")).toHaveTextContent("deck-dispatch-another");
  });

  /**
   * Scenario (review on PR #2222): a kept-worktree notice whose broadcast was
   * stamped with a deck the fleet knows names that deck beside the path — the
   * same checkout path can exist on more than one machine, and the notice
   * outlives whichever deck is selected when it is read.
   */
  it("names the deck that kept the worktree beside the path", () => {
    const kept: KeptWorktreeEntry[] = [
      { id: 1, path: "/home/dev/code/deck-dispatch-issue-1674", confirmedDirty: true, deck: "dev@build-box" },
    ];
    render(<DeckShell runtime={runtimeOf(createFixtureSnapshot("connected"), kept, vi.fn())} initialView={{ kind: "overview" }} />);
    expect(screen.getByTestId("worktree-kept-toast")).toHaveTextContent("Uncommitted work here is KEPT, not deleted: /home/dev/code/deck-dispatch-issue-1674 on dev@build-box.");
  });

  /** Scenario: with nothing kept, no notice renders — the toast stack shows nothing for this feature. */
  it("renders nothing when no worktree was kept", () => {
    render(<DeckShell runtime={runtimeOf(createFixtureSnapshot("connected"), [], vi.fn())} initialView={{ kind: "overview" }} />);
    expect(screen.queryByTestId("worktree-kept-toast")).toBeNull();
  });
});
