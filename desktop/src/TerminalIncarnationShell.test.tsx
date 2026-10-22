import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import type { DeckRuntimeState, DeckSnapshot } from "./types";

/*
 * Every viewport the shell mounts, with the identity it was given. A mount is
 * one xterm and one feed subscription, so a viewport that survives a change of
 * daemon process keeps the previous process's terminal on screen.
 */
const { mounts } = vi.hoisted(() => ({ mounts: [] as { agentId: string; deckId?: string; incarnation?: string }[] }));

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, deckId, incarnation, label }: { agentId: string; deckId?: string; incarnation?: string; label: string }) => {
    useEffect(() => {
      mounts.push({ agentId, deckId, incarnation });
    }, [agentId, deckId, incarnation]);
    return <pre data-testid={`terminal-${agentId}`} data-incarnation={incarnation} aria-label={`${label} terminal`}>terminal</pre>;
  },
}));

import { DeckShell } from "./App";

/**
 * PRD #1666 part 2 (G-fix7) — a terminal on screen belongs to the daemon
 * process that listed its agent. After a deck restart that reuses an agent id,
 * the deck grid's tile and an overview pane each get a viewport for the
 * successor's process, never the one that showed the previous process's
 * output. (`TerminalIncarnation.test.tsx` pins what such a viewport reads.)
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

const planners = () => mounts.filter((mount) => mount.agentId === "planner");

describe("terminal viewports across a deck restart that reuses an agent id", () => {
  beforeEach(() => {
    window.localStorage.clear();
    mounts.length = 0;
  });

  /** Scenario: the deck grid shows Planner's tile for process A; the deck restarts and lists an unrelated Planner under the same id. The tile's terminal is mounted afresh for process B. A snapshot from the same process (control) mounts nothing new. */
  it("mounts the successor's tile terminal afresh for its own process", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "deck" }} />);
    await waitFor(() => expect(planners().at(-1)).toMatchObject({ incarnation: "process-a" }));
    const before = planners().length;

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "deck" }} />); });
    expect(planners(), "the same process keeps its terminal").toHaveLength(before);

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "deck" }} />); });
    expect(planners()).toHaveLength(before + 1);
    expect(planners().at(-1)).toMatchObject({ incarnation: "process-b" });
    expect(screen.getByTestId("terminal-planner")).toHaveAttribute("data-incarnation", "process-b");
  });

  /** Scenario: the user switches process A's Planner tile to its Diff tab; the deck restarts and lists an unrelated Planner under the same id. The successor's tile opens on its terminal, for process B, rather than on the tab chosen for its predecessor (Qodo on PR #1817, finding 21). A snapshot from the same process (control) keeps the chosen tab. */
  it("opens the successor's tile on its terminal, not its predecessor's tab", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "deck" }} />);
    await waitFor(() => expect(screen.getByTestId("terminal-planner")).toBeInTheDocument());
    fireEvent.click(within(screen.getByTestId("agent-tile-planner")).getByRole("tab", { name: "Diff" }));
    expect(screen.queryByTestId("terminal-planner")).toBeNull();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "deck" }} />); });
    expect(screen.queryByTestId("terminal-planner"), "the same process keeps the chosen tab").toBeNull();

    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "deck" }} />); });
    expect(screen.getByTestId("terminal-planner")).toHaveAttribute("data-incarnation", "process-b");
    expect(deck.declared().filter((target) => target.agentId === "planner").at(-1)).toMatchObject({ incarnation: "process-b" });
  });

  /** Scenario: after a restart the user opens the successor's Planner row from the overview. Its pane's terminal is mounted for process B, not for the process that listed the id before. */
  it("opens the successor's pane with a terminal for its own process", async () => {
    const deck = harness();
    const { rerender } = render(<DeckShell runtime={deck.runtime("process-a")} initialView={{ kind: "overview" }} />);
    await act(async () => { rerender(<DeckShell runtime={deck.runtime("process-b")} initialView={{ kind: "overview" }} />); });
    fireEvent.click(screen.getByRole("button", { name: "Open Successor run agent" }));
    const pane = screen.getByTestId("agent-pane-overlay");
    expect(within(pane).getByTestId("terminal-planner")).toHaveAttribute("data-incarnation", "process-b");
    expect(planners().every((mount) => mount.incarnation === "process-b")).toBe(true);
  });
});
