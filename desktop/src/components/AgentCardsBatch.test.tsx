import "../styles.css";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type DeckRuntimeState } from "../types";
import { AgentOverview } from "./AgentOverview";

const INSTANCE = "daemon-instance-batch";
const ROOT_TEXT = "ROOT-EXACT-2399: proceed with the root.\nThen report.";
const CHILD_TEXT = "CHILD-EXACT-2399: proceed with the child.";

// Test-local wire types let this RED suite load before the additive DTOs exist.
type BatchEntry = {
  pane_id: string; agent_id: string; outcome: "included" | "skipped" | "delivered" | "failed";
  name?: string; prompt?: string; action_token?: string; instance_id?: string;
  code?: string; message?: string; message_id?: string;
};
type BatchOptions = { mode: "preview" | "execute"; entries: { pane_id: string; agent_id: string; action_token?: string; instance_id?: string }[]; requestId: string };

const action: AgentCardAction = {
  id: "batch-go", label: "Go ahead", kind: "prompt", prompt: "The local list is not the daemon preview.", prompt_source: "catalog",
  text_input: false, confirm: false, confirm_detail: null, icon: null, placement: "detail", custom: false,
  source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: "tok-local",
};

function entry(id: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: `pane-${id}/${id}`, instance_id: INSTANCE, pane_id: `pane-${id}`, agent_id: id, agent_type: "claude_code", name,
    status: "idle", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null },
    relations: {}, report: null, fields: [], questions: [], actions: [action], primary_action_id: "open",
    facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-23T00:00:00Z", ...overrides,
  };
}

const ROOT = entry("1", "root", { rollup: { total: 2, worst: "idle", counts: { idle: 1, stopped: 1 }, needs_you: 1 } });
const CHILD = entry("2", "child", { parent: ROOT.key, rollup: { total: 1, worst: "stopped", counts: { stopped: 1 }, needs_you: 1 } });
const STOPPED = entry("3", "stopped", { parent: CHILD.key, status: "stopped", actions: [{ ...action, enabled: false, disabled_reason: "agent_stopped" }] });
const OUTSIDE = entry("4", "outside");
const ENTRIES = [ROOT, CHILD, STOPPED, OUTSIDE];
const INCLUDED: BatchEntry[] = [
  { pane_id: ROOT.pane_id, agent_id: ROOT.agent_id, name: ROOT.name, outcome: "included", prompt: ROOT_TEXT, action_token: "tok-preview-root", instance_id: INSTANCE },
  { pane_id: CHILD.pane_id, agent_id: CHILD.agent_id, name: CHILD.name, outcome: "included", prompt: CHILD_TEXT, action_token: "tok-preview-child", instance_id: INSTANCE },
];
const SKIPPED: BatchEntry = { pane_id: STOPPED.pane_id, agent_id: STOPPED.agent_id, name: STOPPED.name, outcome: "skipped", code: "agent_stopped", message: "This agent is no longer running." };

function setup({ partial = false }: { partial?: boolean } = {}) {
  const first = createFixtureFleet("fleet")[0];
  const deck = { ...first, connection: { ...first.connection, incarnation: INSTANCE }, agents: ENTRIES.filter((card) => card !== STOPPED).map((card) => createFixtureStartedAgent({
    id: card.agent_id, daemonId: FIXTURE_DAEMON_ID, displayName: card.name, command: "claude", cwd: "/w",
  })) };
  const invokeCardAction = vi.fn();
  const invokeCardActionBatch = vi.fn(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
    request_id: options.requestId,
    entries: options.mode === "preview" ? [...INCLUDED, SKIPPED] : [
      { ...INCLUDED[0], outcome: "delivered", message_id: "m-batch-root" },
      partial
        ? { ...INCLUDED[1], outcome: "failed", code: "stale_action", message: "The action changed after you reviewed it." }
        : { ...INCLUDED[1], outcome: "delivered", message_id: "m-batch-child" },
    ],
  }));
  const live = {
    mode: "fixture", snapshot: deck, fleet: [deck], terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries: ENTRIES, catalogErrors: [], tree: true, batch: true } },
    invokeCardAction, invokeCardActionBatch,
  } as unknown as DeckRuntimeState;
  render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
  return { invokeCardAction, invokeCardActionBatch };
}

function card(card: AgentCardEntry) {
  const node = document.querySelector<HTMLElement>(`article[data-card-key="${card.key}"]`);
  if (!node) throw new Error(`No card ${card.name}`);
  return within(node);
}

function selectRoot() {
  fireEvent.click(screen.getByTestId("agent-cards-select"));
  fireEvent.click(card(ROOT).getByRole("checkbox", { name: "Select root" }));
}

async function startSelectedBatch() {
  selectRoot();
  await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
  return screen.getByTestId("agent-cards-batch-confirm");
}

const targets = (cards: AgentCardEntry[]) => cards.map(({ pane_id, agent_id }) => ({ pane_id, agent_id }));

beforeEach(() => {
  window.localStorage.clear();
  window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("batch card actions (#2399)", () => {
  /// Scenario: Enter selection mode and select a closed root with a child and grandchild. The selection counts the whole subtree while its cards remain closed; clearing the root clears the selection, and an unrelated root stays unselected.
  it("desktop/agent-cards/028: selecting a closed root selects its whole subtree", async () => {
    const calls = setup();
    expect(document.querySelector(`article[data-card-key="${CHILD.key}"]`)).toBeNull();
    selectRoot();
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("3 selected");
    expect(card(ROOT).getByRole("checkbox", { name: "Select root" })).toBeChecked();
    expect(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" })).not.toBeChecked();
    expect(document.querySelector(`article[data-card-key="${CHILD.key}"]`)).toBeNull();
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
    expect(calls.invokeCardActionBatch).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "preview", entries: targets([ROOT, CHILD, STOPPED]) }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("agent-cards-batch-cancel"));
    fireEvent.click(card(ROOT).getByRole("checkbox", { name: "Select root" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("0 selected");
  });

  /// Scenario: Open a root card's overlay while its subtree is closed, then choose its batch action. The daemon previews the root, child and grandchild, excluding an unrelated root, and nothing is executed.
  it("desktop/agent-cards/029: an overlay batch reaches the card's whole subtree", async () => {
    const calls = setup();
    fireEvent.click(card(ROOT).getByTestId("agent-card-details"));
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-batch-action-batch-go")); });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toBeInTheDocument();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(1);
    expect(calls.invokeCardActionBatch).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "preview", entries: targets([ROOT, CHILD, STOPPED]) }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Start a batch whose action has confirm=false. Its confirmation quotes each daemon-previewed prompt and skipped reason without an editable text box; Cancel executes nothing, and a later Send submits precisely the previewed included tokens and instances.
  it("desktop/agent-cards/030: every batch reviews the daemon's exact text before Send", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    expect(action.confirm).toBe(false);
    const rows = within(dialog).getAllByTestId("agent-cards-batch-entry");
    expect(rows).toHaveLength(3);
    for (const [name, prompt] of [["root", ROOT_TEXT], ["child", CHILD_TEXT]]) {
      const row = rows.find((row) => within(row).queryByText(name, { exact: true }));
      expect(row, `the included agent ${name}`).toBeDefined();
      expect(within(row!).getByTestId("agent-cards-batch-prompt").textContent).toBe(prompt);
    }
    expect(within(dialog).getByText(SKIPPED.message!)).toBeInTheDocument();
    expect(within(dialog).queryByRole("textbox")).toBeNull();
    expect(within(dialog).queryByText(action.prompt!)).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(1);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
    fireEvent.click(within(dialog).getByRole("button", { name: /^Cancel$/ }));
    expect(screen.queryByTestId("agent-cards-batch-confirm")).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(1);
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
    await act(async () => { fireEvent.click(within(screen.getByTestId("agent-cards-batch-confirm")).getByRole("button", { name: /^Send$/ })); });
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(3);
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({
      mode: "execute", entries: INCLUDED.map(({ pane_id, agent_id, action_token, instance_id }) => ({ pane_id, agent_id, action_token, instance_id })),
    }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Send a reviewed batch in which the root succeeds and the child's action has changed. The result names each agent, marks the root delivered and the child failed with the daemon's reason, without claiming that the batch wholly succeeded.
  it("desktop/agent-cards/031: a partial failure shows the result for each agent", async () => {
    const calls = setup({ partial: true });
    const dialog = await startSelectedBatch();
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    const root = rows.find((row) => within(row).queryByText("root", { exact: true }));
    const child = rows.find((row) => within(row).queryByText("child", { exact: true }));
    expect(root).toBeDefined();
    expect(child).toBeDefined();
    expect(within(root!).getByText("Delivered", { exact: true })).toBeInTheDocument();
    expect(within(child!).getByText("Failed", { exact: true })).toBeInTheDocument();
    expect(within(child!).getByText("The action changed after you reviewed it.")).toBeInTheDocument();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });
});
