import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { domIdentity } from "../lib/displayText";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardEntry, type DeckRuntimeState, type EditableCardCatalog } from "../types";
import { AgentOverview } from "./AgentOverview";

/*
 * PR #2491 review (Qodo 2) — the Card actions editor open on a deck whose
 * daemon restarts while the deck stays on the dashboard: the dashboard hands
 * the editor the daemon process its cards come from, and the editor reads its
 * list again from the new one.
 */

const CATALOG: EditableCardCatalog = { actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" };

function card(instance: string): AgentCardEntry {
  return {
    key: "pane-1/1", instance_id: instance, pane_id: "pane-1", agent_id: "1", agent_type: "claude_code", name: "coder",
    status: "idle", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [], primary_action_id: "open",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-26T00:00:00Z",
  } as AgentCardEntry;
}

function runtimeAt(instance: string, readCardCatalog: DeckRuntimeState["readCardCatalog"]): DeckRuntimeState {
  const local = createFixtureFleet("fleet")[0];
  const deck = { ...local, connection: { ...local.connection, incarnation: instance }, agents: [{ ...createFixtureStartedAgent({ id: "1", daemonId: FIXTURE_DAEMON_ID, displayName: "coder", command: "claude", cwd: "/w" }), incarnation: instance }] };
  return {
    mode: "fixture", snapshot: deck, fleet: [deck], terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: instance, revision: 1, entries: [card(instance)], catalogErrors: [], tree: true, batch: true, catalogEdit: true } },
    invokeCardAction: vi.fn(), invokeCardActionBatch: vi.fn(), readCardCatalog, editCardCatalog: vi.fn(),
  } as unknown as DeckRuntimeState;
}

beforeEach(() => window.localStorage.setItem("dot-agent-deck.overview.mode", "cards"));
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("the Card actions editor across a daemon restart (PR #2491 review, Qodo 2)", () => {
  /// Scenario: Open the Card actions editor from the local daemon's ⋯ menu and press Add action. The daemon restarts as another process, and the dashboard's cards now come from it while the deck stays on screen. The open form, made from the old daemon's list, closes and the editor reads the card actions again from the new daemon; a redraw from the same process reads nothing more.
  it("reads the card actions again when the deck's daemon process changes", async () => {
    const readCardCatalog = vi.fn(async () => structuredClone(CATALOG));
    const view = render(<AgentOverview runtime={runtimeAt("process-a", readCardCatalog)} onNavigate={vi.fn()} />);
    const section = screen.getAllByTestId("daemon-group").find((node) => node.getAttribute("data-daemon-id") === domIdentity(FIXTURE_DAEMON_ID))!;
    fireEvent.click(within(section).getByTestId("daemon-more"));
    await act(async () => { fireEvent.click(within(section).getByRole("menuitem", { name: "Card actions" })); });
    const editor = screen.getByRole("dialog", { name: /^Card actions: / });
    expect(readCardCatalog).toHaveBeenCalledTimes(1);
    fireEvent.click(within(editor).getByRole("button", { name: "Add action" }));
    expect(within(editor).getByLabelText("Name")).toBeInTheDocument();

    await act(async () => { view.rerender(<AgentOverview runtime={runtimeAt("process-b", readCardCatalog)} onNavigate={vi.fn()} />); });
    expect(readCardCatalog).toHaveBeenCalledTimes(2);
    const reopened = screen.getByRole("dialog", { name: /^Card actions: / });
    expect(within(reopened).queryByLabelText("Name"), "the form made from the old list is gone").toBeNull();

    await act(async () => { view.rerender(<AgentOverview runtime={runtimeAt("process-b", readCardCatalog)} onNavigate={vi.fn()} />); });
    expect(readCardCatalog).toHaveBeenCalledTimes(2);
  });
});
