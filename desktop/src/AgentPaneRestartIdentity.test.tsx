import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import { AGENTS_STOPPED_BY_RESTART } from "./lib/agentIdentity";
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

/**
 * PRD #1666 part 2 (G-fix6) — an open agent pane is bound to the incarnation
 * of the record it was opened on. A restarted deck numbers its agents from 1
 * again, so its next snapshot can list an unrelated agent under the open id;
 * that snapshot must not turn the open pane into the newcomer's.
 *
 * The deck is the fixture's one local deck, listed first by process A and then
 * by its same-build replacement B, whose Planner is a different agent with the
 * same id (named "Successor" so the test can tell the two apart). `empty` is B
 * listing nobody, which is what a restarted deck usually lists.
 *
 * G-fix7c (the user's decision): a pane of an agent the restart stopped
 * closes, no stop, close or cleanup is asked of the deck, and one notice names what went.
 */
function deckOf(process: "process-a" | "process-b", empty = false): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return {
    ...snapshot,
    connection: { ...snapshot.connection, incarnation: process },
    agents: empty ? [] : snapshot.agents.map((agent) => ({
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
  const runtime = (process: "process-a" | "process-b", empty = false): DeckRuntimeState => {
    const deck = deckOf(process, empty);
    return { ...base, snapshot: deck, fleet: [deck] } as unknown as DeckRuntimeState;
  };
  /** Every target the shell declared shown, flattened. */
  const declared = () => (setShownTerminals.mock.calls as unknown as [{ deckId: string; agentId: string; incarnation?: string }[]][]).flatMap(([targets]) => targets);
  return { runtime, setShownTerminals, sendTerminalInput, declared, runAction: base.runAction, reconnect: base.reconnect };
}

/** Every agent process A listed, as the notice names it: the fixture's are all dashboard agents. */
const processANames = () => deckOf("process-a").agents.map((agent) => agent.displayName);
const expectRestartNotice = () => {
  const notice = screen.getByTestId("restart-toast");
  expect(notice).toHaveTextContent(`${AGENTS_STOPPED_BY_RESTART}: ${processANames().join(", ")}.`);
  return notice;
};

const openPlanner = (name = "Plan / architecture") => fireEvent.click(screen.getByRole("button", { name: `Open ${name} agent` }));

describe("an open pane across a deck restart that reuses its agent id", () => {
  beforeEach(() => window.localStorage.clear());

  /** Scenario: open Planner from the overview, then the deck restarts and its first snapshot lists an unrelated Planner under the same id. The pane closes rather than becoming the newcomer's, one notice names every agent the restart stopped, nothing is declared for the newcomer, and no stop, close or cleanup is asked of the deck. */
  it("closes an overview pane of an agent the restart stopped, and names it in the notice", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(deck.declared().at(-1)).toMatchObject({ agentId: "planner", incarnation: "process-a" });
    const declaredBefore = deck.setShownTerminals.mock.calls.length;

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "overview" }} />); });

    expect(screen.queryByTestId("agent-pane-overlay"), "the pane of a stopped agent must close").not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Successor run" }), "the open view must not become the successor's agent").toBeNull();
    expect(screen.getAllByTestId("restart-toast")).toHaveLength(1);
    expectRestartNotice();
    expect(deck.sendTerminalInput).not.toHaveBeenCalled();
    expect(deck.runAction, "dropping the stopped agents must send the deck nothing").not.toHaveBeenCalled();
    expect(deck.reconnect).not.toHaveBeenCalled();
    expect(deck.declared().filter((target) => target.incarnation === "process-b"), "nothing may declare the successor's terminal under the old view").toEqual([]);
    expect(deck.setShownTerminals.mock.calls.slice(declaredBefore).at(-1)).toEqual([[]]);
  });

  /** Scenario: the deck restarts and lists nobody at all — the usual restart. The open pane closes and the notice still names every agent the restart stopped; dismissing it removes it. */
  it("names the stopped agents when the restarted deck lists nobody", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b", true)} initialView={{ kind: "overview" }} />); });
    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    const notice = expectRestartNotice();
    expect(deck.runAction).not.toHaveBeenCalled();
    fireEvent.click(within(notice).getByRole("button", { name: "Dismiss restart notice" }));
    expect(screen.queryByTestId("restart-toast")).not.toBeInTheDocument();
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
    expect(screen.queryByTestId("restart-toast"), "a snapshot from the same process is no restart").not.toBeInTheDocument();
  });

  /** Scenario: after the pane closed with the restart, the user opens the successor's own Planner row. That is a fresh selection, so it opens the successor with a live terminal declared for its process. */
  it("opens the successor's agent when the user selects its row", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    openPlanner();
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "overview" }} />); });
    await waitFor(() => expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument());

    openPlanner("Successor run");
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByRole("heading", { name: "Successor run" })).toBeVisible();
    expect(within(pane).getByTestId("terminal-planner")).toBeVisible();
    expect(deck.declared().at(-1)).toMatchObject({ agentId: "planner", incarnation: "process-b" });
    fireEvent.click(within(pane).getByRole("button", { name: /Type into/ }));
    expect(deck.sendTerminalInput).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, agentId: "planner", incarnation: "process-b" }, "typed");
  });

  /** Scenario: open Planner using only its deck and agent ids over either screen, immediately or before its first listing arrives. The first named listing opens its own terminal (control); after a restart reuses that id, the old pane closes without becoming the successor's, and one notice names the stopped agents. */
  it.each([
    ["deck", false], ["overview", false], ["deck", true], ["overview", true],
  ] as const)("closes an ids-only %s pane over a reused id and says why (late first listing: %s)", async (from, late) => {
    const deck = harness();
    const view = { kind: "agent" as const, deckId: FIXTURE_DAEMON_ID, agentId: "planner", from };
    const first = deck.runtime("process-a");
    const unseen = { ...first.snapshot, agents: [], connection: { ...first.snapshot.connection, status: "disconnected" as const, incarnation: undefined } };
    const { rerender } = render(<DeckShell runtime={late ? { ...first, snapshot: unseen, fleet: [unseen] } : first} initialView={view} />);
    if (late) {
      expect(deck.declared()).toEqual([]);
      await act(async () => { rerender(<DeckShell runtime={first} initialView={view} />); });
    }
    expect(within(screen.getByTestId("agent-pane-overlay")).getByRole("heading", { name: "Plan / architecture" })).toBeVisible();
    expect(deck.declared()).toContainEqual(expect.objectContaining({ agentId: "planner", incarnation: "process-a" }));

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={view} />); });

    expect(screen.queryByTestId("agent-pane-overlay")).not.toBeInTheDocument();
    if (from === "deck") expect(screen.getByTestId("agent-tile-successor")).toHaveAttribute("data-presentation", "tile");
    expect(screen.getAllByTestId("restart-toast")).toHaveLength(1);
    expectRestartNotice();
    if (from === "overview") expect(deck.declared().filter((target) => target.incarnation === "process-b")).toEqual([]);
    expect(deck.sendTerminalInput).not.toHaveBeenCalled();
    expect(screen.queryByTestId("toast"), "the restart notice is the one sentence about the pane").not.toBeInTheDocument();
    expect(deck.runAction).not.toHaveBeenCalled();
  });
});
