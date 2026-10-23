import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureSnapshot } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, LOCAL_ENDPOINT_SELECTION, type DesktopSettingsDto } from "./lib/bridge";
import { AGENTS_STOPPED_BY_RESTART } from "./lib/agentIdentity";
import type { AgentSession, DeckRuntimeState, DeckSnapshot } from "./types";

vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId }: { agentId: string }) => <pre data-testid={`terminal-${agentId}`}>terminal</pre>,
}));

import { DeckShell } from "./App";

/**
 * PRD #1666 part 2 (G-fix7d) — the desktop's restart toast holds every agent
 * deck restarts stopped until the user dismisses it, as the TUI's footer
 * notice does (CLAUDE.md rule 22): a later restart joins it rather than
 * replacing names the user has not seen, and a long list is shown whole.
 */
function deckOf(process: string, agents: (agents: AgentSession[]) => AgentSession[] = (all) => all): DeckSnapshot {
  const snapshot = createFixtureSnapshot("connected");
  return {
    ...snapshot,
    connection: { ...snapshot.connection, incarnation: process },
    agents: agents(snapshot.agents).map((agent) => ({ ...agent, incarnation: process })),
  };
}

function runtimeOf(deck: DeckSnapshot): DeckRuntimeState {
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
  } as unknown as DeckRuntimeState;
}

/** Process B lists one agent of its own, which a restart to C then stops. */
const onlyB = (agents: AgentSession[]) => agents.slice(0, 1).map((agent) => ({ ...agent, id: "b-only", displayName: "b-only-agent" }));

describe("the restart toast", () => {
  beforeEach(() => window.localStorage.clear());

  /** Scenario: the deck restarts twice before the user dismisses the notice: process A's agents are stopped, then process B's one agent. The one toast names A's agents and B's, rather than only the latest restart's. Dismissed in between (control), the second restart's toast names only B's agent. */
  it.each([false, true])("joins a later restart's agents to the notice (dismissed in between: %s)", async (dismissed) => {
    const first = deckOf("process-a");
    const { rerender } = render(<DeckShell runtime={runtimeOf(first)} initialView={{ kind: "overview" }} />);
    await act(async () => { rerender(<DeckShell runtime={runtimeOf(deckOf("process-b", onlyB))} initialView={{ kind: "overview" }} />); });
    const aNames = first.agents.map((agent) => agent.displayName).join(", ");
    expect(screen.getByTestId("restart-toast")).toHaveTextContent(`${AGENTS_STOPPED_BY_RESTART}: ${aNames}.`);
    if (dismissed) fireEvent.click(within(screen.getByTestId("restart-toast")).getByRole("button", { name: "Dismiss restart notice" }));

    await act(async () => { rerender(<DeckShell runtime={runtimeOf(deckOf("process-c", () => []))} initialView={{ kind: "overview" }} />); });
    expect(screen.getAllByTestId("restart-toast")).toHaveLength(1);
    expect(screen.getByTestId("restart-toast")).toHaveTextContent(
      dismissed ? `${AGENTS_STOPPED_BY_RESTART}: b-only-agent.` : `${AGENTS_STOPPED_BY_RESTART}: ${aNames}, b-only-agent.`,
    );
  });

  /** Scenario: a restart stops forty agents with long names — a notice far longer than any one sentence the toast shows elsewhere. The toast names every one of them, the last included, with no elision. */
  it("names every agent of a long list", async () => {
    const template = createFixtureSnapshot("connected").agents[0];
    const many = deckOf("process-a", () => Array.from({ length: 40 }, (_, i) => ({ ...template, id: `agent-${i}`, displayName: `long-running-agent-${String(i).padStart(2, "0")}-${"x".repeat(60)}` })));
    const { rerender } = render(<DeckShell runtime={runtimeOf(many)} initialView={{ kind: "overview" }} />);
    await act(async () => { rerender(<DeckShell runtime={runtimeOf(deckOf("process-b", () => []))} initialView={{ kind: "overview" }} />); });
    const toast = screen.getByTestId("restart-toast");
    const names = many.agents.map((agent) => agent.displayName);
    expect(names.join(", ").length).toBeGreaterThan(2048);
    expect(toast).toHaveTextContent(`${AGENTS_STOPPED_BY_RESTART}: ${names.join(", ")}.`);
    expect(toast.textContent).not.toContain("…");
  });
});
