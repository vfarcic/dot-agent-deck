import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { OVERVIEW_MODE_STORAGE_KEY } from "../lib/agentCards";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { deckName, domIdentity } from "../lib/displayText";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckRuntimeState } from "../types";
import { AgentOverview } from "./AgentOverview";

/*
 * PR #2491 (the maintainer, 2026-11-25) — a shortcut to the Card actions
 * editor in the floating selection bar, beside the batch actions: with every
 * selected card on one daemon it opens that daemon's editor; with the
 * selection across daemons it opens a small menu of those daemons
 * (`desktop/agent-cards/116`, `117`). The daemon's ⋯ menu stays the main way in.
 */

const INSTANCE = "daemon-instance-shortcuts";
const REMOTE_INSTANCE = "daemon-instance-shortcuts-remote";
const key = (id: string) => `pane-${id}/${id}`;

const goAhead: AgentCardAction = {
  id: "go-ahead", label: "Go ahead", kind: "prompt", prompt: "Go ahead.", prompt_source: "catalog", text_input: false, confirm: false,
  confirm_detail: null, icon: "play", placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-go",
};

function entry(agentId: string, name: string, instance = INSTANCE): AgentCardEntry {
  return {
    key: key(agentId), instance_id: instance, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "idle", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: null, fields: [], questions: [], actions: [goAhead], primary_action_id: "go-ahead",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-25T00:00:00Z",
  };
}

const LOCAL_CARDS = [entry("1", "local-coder"), entry("2", "local-tester")];
const REMOTE_CARDS = [entry("1", "far-coder", REMOTE_INSTANCE)];

/** Two daemons whose cards do batches; the second is from a release whose card actions cannot be edited unless `remoteEditable`. */
function runtime({ remoteEditable = false }: { remoteEditable?: boolean } = {}): DeckRuntimeState {
  const [local, far] = createFixtureFleet("fleet");
  const agentsOf = (list: AgentCardEntry[], daemonId: string, incarnation: string) => list.map((card) => ({ ...createFixtureStartedAgent({ id: card.agent_id, daemonId, displayName: card.name, command: "claude", cwd: "/w" }), incarnation }));
  const fleet = [
    { ...local, connection: { ...local.connection, incarnation: INSTANCE }, agents: agentsOf(LOCAL_CARDS, FIXTURE_DAEMON_ID, INSTANCE) },
    { ...far, connection: { ...far.connection, incarnation: REMOTE_INSTANCE }, agents: agentsOf(REMOTE_CARDS, FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE) },
  ];
  const snapshot = (deckId: string, instanceId: string, entries: AgentCardEntry[], catalogEdit: boolean): AgentCardsSnapshot => ({ deckId, instanceId, revision: 1, entries, catalogErrors: [], tree: true, batch: true, catalogEdit });
  return {
    mode: "fixture", snapshot: fleet[0], fleet, terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: {
      [FIXTURE_DAEMON_ID]: snapshot(FIXTURE_DAEMON_ID, INSTANCE, LOCAL_CARDS, true),
      [FIXTURE_REMOTE_DAEMON_ID]: snapshot(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE, REMOTE_CARDS, remoteEditable),
    },
    invokeCardAction: vi.fn(), invokeCardActionBatch: vi.fn(),
    readCardCatalog: vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" })),
    editCardCatalog: vi.fn(),
  } as unknown as DeckRuntimeState;
}

const [LOCAL_DECK, FAR_DECK] = createFixtureFleet("fleet");
const LOCAL_NAME = deckName(LOCAL_DECK.connection);
const FAR_NAME = deckName(FAR_DECK.connection);

/** Tick the checkbox of the card called `name` on daemon `deckId`. */
function tick(deckId: string, name: string) {
  const card = [...document.querySelectorAll<HTMLElement>("article[data-card-key]")]
    .find((node) => node.closest("[data-testid='daemon-group']")?.getAttribute("data-daemon-id") === domIdentity(deckId) && within(node).queryByRole("checkbox", { name: `Select ${name}` }));
  if (!card) throw new Error(`No card ${name} on ${deckId}`);
  fireEvent.click(within(card).getByRole("checkbox", { name: `Select ${name}` }));
}

const bar = () => screen.getByTestId("agent-cards-selection-bar");

beforeEach(() => {
  window.localStorage.clear();
  window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => {
  cleanup();
  window.localStorage.clear();
  act(() => setDashboardFilter(clearDashboardFilter()));
});

describe("Edit card actions in the selection bar (PR #2491, 2026-11-25)", () => {
  /// Scenario: Tick two cards on the local daemon. The selection bar shows a sliders button named and titled "Edit card actions" after the batch action icons, apart from them; pressing it opens the local daemon's Card actions editor and reads its actions. Clear, tick the other daemon's card, whose release cannot edit card actions, and press it again: that daemon's editor opens read-only, as from its ⋯ menu, and nothing is read.
  it("desktop/agent-cards/116: a selection on one daemon opens that daemon's Card actions editor", async () => {
    const live = runtime();
    render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    tick(FIXTURE_DAEMON_ID, "local-coder");
    tick(FIXTURE_DAEMON_ID, "local-tester");
    const edit = within(bar()).getByRole("button", { name: "Edit card actions" });
    expect(edit).toHaveAttribute("title", "Edit card actions");
    expect(edit).toHaveAttribute("type", "button");
    expect(edit).not.toHaveAttribute("aria-haspopup");
    expect(edit.closest("[data-icon]")?.getAttribute("data-icon")).toBe("sliders-horizontal");
    // After the batch action icon group, and not one of its actions.
    const group = within(bar()).getByRole("group", { name: "Send to the 2 selected cards" });
    expect(group.contains(edit)).toBe(false);
    expect(group.compareDocumentPosition(edit) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(edit.closest(".agent-cards-selection-edit")).not.toBeNull();

    await act(async () => { fireEvent.click(edit); });
    const editor = screen.getByRole("dialog", { name: `Card actions: ${LOCAL_NAME}` });
    expect(within(editor).queryByTestId("agent-card-actions-editor-readonly")).toBeNull();
    expect(live.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, INSTANCE);
    fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close"));

    fireEvent.click(within(bar()).getByRole("button", { name: "Clear" }));
    tick(FIXTURE_REMOTE_DAEMON_ID, "far-coder");
    await act(async () => { fireEvent.click(within(bar()).getByRole("button", { name: "Edit card actions" })); });
    const readOnly = screen.getByRole("dialog", { name: `Card actions: ${FAR_NAME}` });
    expect(within(readOnly).getByTestId("agent-card-actions-editor-readonly")).toHaveTextContent(/earlier release, so its card actions cannot be edited from the app/);
    expect(live.readCardCatalog).toHaveBeenCalledTimes(1);
  });

  /// Scenario: Tick a card on each of two daemons. The selection bar's Edit card actions is a menu button; pressing it opens a small menu named "Edit card actions" listing the two daemons by the names the dashboard gives them, in the dashboard's order, with the first focused; ArrowDown moves to the second, Escape closes the menu and gives focus back to the button, and choosing the second daemon closes the menu and opens that daemon's Card actions editor.
  it("desktop/agent-cards/117: a selection across daemons asks which daemon's Card actions to edit", async () => {
    const live = runtime({ remoteEditable: true });
    render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    tick(FIXTURE_DAEMON_ID, "local-coder");
    tick(FIXTURE_REMOTE_DAEMON_ID, "far-coder");
    const edit = within(bar()).getByRole("button", { name: "Edit card actions" });
    expect(edit).toHaveAttribute("title", "Edit card actions");
    expect(edit).toHaveAttribute("aria-haspopup", "menu");
    expect(edit).toHaveAttribute("aria-expanded", "false");
    expect(edit.closest("[data-icon]")?.getAttribute("data-icon")).toBe("sliders-horizontal");
    expect(screen.queryByRole("dialog", { name: /^Card actions: / })).toBeNull();

    fireEvent.click(edit);
    expect(edit).toHaveAttribute("aria-expanded", "true");
    const menu = within(bar()).getByRole("menu", { name: "Edit card actions" });
    expect(edit).toHaveAttribute("aria-controls", menu.id);
    const items = within(menu).getAllByRole("menuitem");
    expect(items.map((item) => item.textContent)).toEqual([LOCAL_NAME, FAR_NAME]);
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(menu, { key: "ArrowDown" });
    expect(items[1]).toHaveFocus();
    fireEvent.keyDown(menu, { key: "Escape" });
    expect(within(bar()).queryByRole("menu")).toBeNull();
    expect(edit).toHaveFocus();
    expect(live.readCardCatalog).not.toHaveBeenCalled();

    fireEvent.keyDown(edit, { key: "Enter" });
    const reopened = within(bar()).getByRole("menu", { name: "Edit card actions" });
    await act(async () => { fireEvent.click(within(reopened).getByRole("menuitem", { name: FAR_NAME })); });
    expect(within(bar()).queryByRole("menu")).toBeNull();
    expect(screen.getByRole("dialog", { name: `Card actions: ${FAR_NAME}` })).toBeInTheDocument();
    expect(live.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_REMOTE_DAEMON_ID, REMOTE_INSTANCE);
  });
});
