import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label, onInput }: { agentId: string; label: string; onInput: (data: string) => void }) => (
    <pre data-testid={`terminal-${agentId}`} aria-label={`${label} terminal`}>
      terminal
      <button aria-label={`Type into ${label} terminal`} onClick={() => onInput("typed")}>type</button>
    </pre>
  ),
}));

import { DeckShell } from "./App";

const GONE = "that agent is gone (the deck restarted)";

/**
 * PRD #1666 part 2 (G-fix6) — an open agent pane is bound to the incarnation
 * of the record it was opened on. A restarted deck numbers its agents from 1
 * again, so its next snapshot can list an unrelated agent under the open id;
 * that snapshot must not turn the open pane into the newcomer's.
 *
 * The deck is the fixture's one local deck, listed first by process A and then
 * by its same-build replacement B, whose Planner is a different agent with the
 * same id (named "Successor" so the test can tell the two apart).
 */
function deckOf(process: "process-a" | "process-b"): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return {
    ...snapshot,
    agents: snapshot.agents.map((agent) => ({
      ...agent,
      incarnation: process,
      ...(process === "process-b" && agent.id === "planner" ? { role: "Successor", displayName: "Successor run" } : {}),
      ...(agent.id === "planner" ? { status: "running" as const, writeLease: "write" as const } : {}),
    })),
  };
}

function harness() {
  const settings: DesktopSettingsDto = { ...structuredClone(DEFAULT_DESKTOP_SETTINGS), endpoints: { remote: [], selection: LOCAL_ENDPOINT_SELECTION } };
  const setShownTerminals = vi.fn(async () => undefined);
  const sendTerminalInput = vi.fn(async () => undefined);
  const base = {
    mode: "live",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(async () => ({ ok: true })),
    sendTerminalInput,
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals,
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: vi.fn(async () => ({ settings: structuredClone(settings), path: "/tmp/desktop.toml" })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => structuredClone(next)),
  };
  const runtime = (process: "process-a" | "process-b"): DeckRuntimeState => {
    const deck = deckOf(process);
    return { ...base, snapshot: deck, fleet: [deck] } as unknown as DeckRuntimeState;
  };
  /** Every target the shell declared shown, flattened. */
  const declared = () => (setShownTerminals.mock.calls as unknown as [{ deckId: string; agentId: string; incarnation?: string }[]][]).flatMap(([targets]) => targets);
  return { runtime, setShownTerminals, sendTerminalInput, declared };
}

const openPlanner = (name = "Plan / architecture") => fireEvent.click(screen.getByRole("button", { name: `Open ${name} agent` }));

describe("an open pane across a deck restart that reuses its agent id", () => {
  beforeEach(() => window.localStorage.clear());

  /** Scenario: open Planner from the overview, then the deck restarts and its first snapshot lists an unrelated Planner under the same id. The pane stays on the old Planner's last record and says the agent is gone; it mounts no terminal, declares none for the newcomer and takes no input. */
  it("keeps an overview pane on the agent it opened and says it is gone", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(deck.declared().at(-1)).toMatchObject({ agentId: "planner", incarnation: "process-a" });
    const declaredBefore = deck.setShownTerminals.mock.calls.length;

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "overview" }} />); });

    const after = screen.getByTestId("agent-pane-overlay");
    expect(within(after).getByRole("heading", { name: "Planner" })).toBeVisible();
    expect(within(after).queryByRole("heading", { name: "Successor" }), "the open pane must not become the successor's agent").toBeNull();
    expect(within(after).queryByTestId("terminal-planner"), "no terminal may be mounted for a gone agent").toBeNull();
    expect(after.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "agent-gone");
    expect(after).toHaveTextContent(GONE);
    expect(within(after).queryByRole("button", { name: /Type into/ })).toBeNull();
    expect(deck.sendTerminalInput).not.toHaveBeenCalled();
    expect(deck.declared().filter((target) => target.incarnation === "process-b"), "nothing may declare the successor's terminal under the old view").toEqual([]);
    expect(deck.setShownTerminals.mock.calls.slice(declaredBefore).at(-1)).toEqual([[]]);
  });

  /** Scenario: the control — the same deck process sends a fresh snapshot of the same agents. The open pane keeps its live terminal and keeps declaring it. */
  it("keeps the pane live across a snapshot from the same process", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />); });
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(pane.querySelector(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    expect(deck.declared().at(-1)).toMatchObject({ agentId: "planner", incarnation: "process-a" });
  });

  /** Scenario: after the gone pane is closed, the user opens the successor's own Planner row. That is a fresh selection, so it opens the successor with a live terminal declared for its process. */
  it("opens the successor's agent when the user selects its row", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "overview" }} />); });
    fireEvent.click(within(screen.getByTestId("agent-pane-overlay")).getByRole("button", { name: "Back to dashboard" }));
    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());

    openPlanner("Successor run");
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Successor" })).toBeVisible();
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(deck.declared().at(-1)).toMatchObject({ agentId: "planner", incarnation: "process-b" });
    fireEvent.click(within(pane).getByRole("button", { name: /Type into/ }));
    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: "process-b" }, "typed");
  });

  /** Scenario: a Planner pane opened over the Daemons screen, then the deck restarts and lists an unrelated Planner under the same id. The pane closes rather than promoting the newcomer's tile, and the toast says the agent is gone. */
  it("closes a deck-origin pane over a reused id and says why", async () => {
    const deck = harness();
    const view = { kind: "agent" as const, deckId: FIXTURE_DAEMON_ID, agentId: "planner", from: "deck" as const };
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={view} />);
    expect(within(screen.getByTestId("agent-pane-overlay")).getByRole("heading", { name: "Planner" })).toBeVisible();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={view} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("agent-tile-successor")).toHaveAttribute("data-presentation", "tile");
    expect(screen.getByTestId("toast")).toHaveTextContent(GONE);
  });
});
