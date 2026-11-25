import "../styles.css";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID, FIXTURE_REMOTE_DAEMON_ID } from "../data/fixture";
import { deckName } from "../lib/displayText";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type DeckRuntimeState } from "../types";
import { BATCH_DAEMON_UNREACHABLE, BATCH_NOT_KNOWN } from "../hooks/useAgentCardsView";
import { AgentOverview } from "./AgentOverview";

const INSTANCE = "daemon-instance-batch";
const ROOT_TEXT = "ROOT-EXACT-2399: proceed with the root.\nThen report.";
const CHILD_TEXT = "CHILD-EXACT-2399: proceed with the child.";

// Test-local wire types let this RED suite load before the additive DTOs exist.
type BatchEntry = {
  pane_id: string; agent_id: string; outcome: "included" | "skipped" | "delivered" | "failed";
  name?: string; prompt?: string; prompt_source?: string; action_token?: string; instance_id?: string;
  code?: string; message?: string; message_id?: string; state?: string;
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

function setup({ partial = false, entries = ENTRIES, preview = [...INCLUDED, SKIPPED], batch = true }: { partial?: boolean; entries?: AgentCardEntry[]; preview?: BatchEntry[]; batch?: boolean } = {}) {
  const first = createFixtureFleet("fleet")[0];
  const deck = { ...first, connection: { ...first.connection, incarnation: INSTANCE }, agents: entries.filter((card) => card.status !== "stopped").map((card) => createFixtureStartedAgent({
    id: card.agent_id, daemonId: FIXTURE_DAEMON_ID, displayName: card.name, command: "claude", cwd: "/w",
  })) };
  const invokeCardAction = vi.fn();
  const invokeCardActionBatch = vi.fn(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
    request_id: options.requestId,
    entries: options.mode === "preview" ? preview : [
      { ...INCLUDED[0], outcome: "delivered", message_id: "m-batch-root", state: "queued" },
      partial
        ? { ...INCLUDED[1], outcome: "failed", code: "stale_action", message: "The action changed after you reviewed it." }
        : { ...INCLUDED[1], outcome: "delivered", message_id: "m-batch-child", state: "queued" },
    ],
  }));
  const live = {
    mode: "fixture", snapshot: deck, fleet: [deck], terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries, catalogErrors: [], tree: true, batch } },
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

/*
 * PRD #2374 decision 2026-11-24 (3): the selection toolbar's batch buttons are
 * icons, named by the action's label. The tests find them by
 * that name, which a text button and an icon button both carry; what the icon
 * form itself must look like is `desktop/agent-cards/051`'s.
 */
const selectionAction = (label: string) => within(screen.getByTestId("agent-cards-selection-bar")).getByRole("button", { name: label });

/** PR #2491 decision 2026-11-25 — the skipped agents are one collapsed line: open it to read each one. */
function showSkipped(dialog: HTMLElement) {
  const toggle = within(dialog).queryByTestId("agent-cards-batch-skipped-toggle");
  if (toggle && toggle.getAttribute("aria-expanded") === "false") fireEvent.click(toggle);
}

function selectRoot() {
  fireEvent.click(card(ROOT).getByRole("checkbox", { name: "Select root" }));
}

async function startSelectedBatch() {
  selectRoot();
  await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
  return screen.getByTestId("agent-cards-batch-confirm");
}

/*
 * Issue #2399 review 1 / audit S1 — two daemons whose cards have the SAME pane
 * and agent ids (they are daemon-local) and different names. Execution fails
 * on the local daemon with no `name`, the daemon's normal failure shape.
 */
const REMOTE_INSTANCE = "daemon-instance-batch-remote";
const SHARED = { pane_id: "pane-shared", agent_id: "shared" };

function setupTwoDaemons({ localActions = [action] }: { localActions?: AgentCardAction[] } = {}) {
  const [local, remote] = createFixtureFleet("fleet");
  const fleet = [
    { ...local, connection: { ...local.connection, incarnation: INSTANCE } },
    { ...remote, connection: { ...remote.connection, incarnation: REMOTE_INSTANCE } },
  ];
  const localCard = entry("shared", "local-coder", { key: "pane-shared/shared", pane_id: SHARED.pane_id, actions: localActions });
  const remoteCard = entry("shared", "remote-coder", { key: "pane-shared/shared", pane_id: SHARED.pane_id, instance_id: REMOTE_INSTANCE });
  const invokeCardActionBatch = vi.fn(async (deckId: string, _actionId: string, options: BatchOptions) => {
    const onLocal = deckId === FIXTURE_DAEMON_ID;
    const name = onLocal ? "local-coder" : "remote-coder";
    return {
      request_id: options.requestId,
      entries: options.mode === "preview"
        ? [{ ...SHARED, name, outcome: "included", prompt: `${name}: go ahead.`, action_token: `tok-${name}`, instance_id: onLocal ? INSTANCE : REMOTE_INSTANCE }]
        : [onLocal
          ? { ...SHARED, outcome: "failed", code: "stale_action", message: "The action changed after you reviewed it." }
          : { ...SHARED, name, outcome: "delivered", message_id: "m-remote", state: "queued" }],
    };
  });
  const live = {
    mode: "fixture", snapshot: fleet[0], fleet, terminalData: {}, clearError: vi.fn(), runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined), resizeTerminal: vi.fn(async () => undefined), setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined), listProjects: vi.fn(async () => ({ projects: [] })), resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(), saveSettings: vi.fn(), testEndpoint: vi.fn(), secretStatus: vi.fn(), storeSecret: vi.fn(), forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level), desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: {
      [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 1, entries: [localCard], catalogErrors: [], tree: true, batch: true },
      [FIXTURE_REMOTE_DAEMON_ID]: { deckId: FIXTURE_REMOTE_DAEMON_ID, instanceId: REMOTE_INSTANCE, revision: 1, entries: [remoteCard], catalogErrors: [], tree: true, batch: true },
    },
    invokeCardAction: vi.fn(), invokeCardActionBatch,
  } as unknown as DeckRuntimeState;
  const { rerender } = render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
  /* A daemon's next answer advertising batches or not, as after a downgrade or a reconnect to an older daemon. */
  let current = live;
  const setBatch = (deckId: string, batch: boolean) => {
    const cards = current.agentCards!;
    current = { ...current, agentCards: { ...cards, [deckId]: { ...cards[deckId]!, revision: cards[deckId]!.revision + 1, batch } } } as DeckRuntimeState;
    rerender(<AgentOverview runtime={current} onNavigate={vi.fn()} />);
  };
  return { invokeCardActionBatch, setBatch, localName: deckName(fleet[0].connection), remoteName: deckName(fleet[1].connection) };
}

const targets = (cards: AgentCardEntry[]) => cards.map(({ pane_id, agent_id }) => ({ pane_id, agent_id }));

beforeEach(() => {
  window.localStorage.clear();
  window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
  act(() => setDashboardFilter(clearDashboardFilter()));
});
afterEach(() => { cleanup(); window.localStorage.clear(); });

describe("batch card actions (#2399)", () => {
  /// Scenario: Tick the checkbox of a closed root with a child and grandchild. The selection counts the whole subtree while its cards remain closed; clearing the root clears the selection and its bar, and an unrelated root stays unselected.
  it("desktop/agent-cards/028: selecting a closed root selects its whole subtree", async () => {
    const calls = setup();
    expect(document.querySelector(`article[data-card-key="${CHILD.key}"]`)).toBeNull();
    selectRoot();
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("3 selected");
    expect(card(ROOT).getByRole("checkbox", { name: "Select root" })).toBeChecked();
    expect(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" })).not.toBeChecked();
    expect(document.querySelector(`article[data-card-key="${CHILD.key}"]`)).toBeNull();
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    expect(calls.invokeCardActionBatch).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "preview", entries: targets([ROOT, CHILD, STOPPED]) }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("agent-cards-batch-cancel"));
    fireEvent.click(card(ROOT).getByRole("checkbox", { name: "Select root" }));
    // PR #2491 review: an empty selection has no bar.
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
  });

  /// Scenario: Open the Cards view on a daemon that does batches. Every card shows its checkbox with no Select button to press, nothing selected shows no selection bar, and ticking a card opens neither its agent's pane nor anything else but shows a bar at the foot with the count, the actions as icons inside one group, and Clear, which empties the selection and takes the bar away. A card's own icons and the bar's sit in the same kind of group, Needs you is the compact bell with its words kept, and a daemon without batches shows no checkbox and no bar.
  it("desktop/agent-cards/085: checkboxes are always there, and a bar at the foot appears with the selection", async () => {
    const onCard = { ...action, id: "card-merge", label: "Merge it", placement: "card" as const, icon: "git-merge", token: "tok-merge" };
    const needy = { ...OUTSIDE, needs_you: true, needs_you_reasons: ["waiting"], actions: [onCard, action] };
    const calls = setup({ entries: [{ ...ROOT, actions: [onCard, action] }, CHILD, STOPPED, needy] });
    expect(screen.queryByTestId("agent-cards-select"), "no Select button").toBeNull();
    expect(screen.queryByRole("button", { name: /^Select$/ })).toBeNull();
    expect(screen.queryByTestId("agent-cards-selection"), "the old selection line under the toolbar is gone").toBeNull();
    expect(card(ROOT).getByRole("checkbox", { name: "Select root" })).not.toBeChecked();
    expect(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" })).not.toBeChecked();
    expect(screen.queryByTestId("agent-cards-selection-bar"), "nothing selected, no bar").toBeNull();

    // Needs you is the compact bell, its words kept for the tooltip and a screen reader.
    const bell = card(OUTSIDE).getByTestId("agent-card-needs-you");
    expect(bell).toHaveClass("is-compact");
    expect(bell).toHaveAttribute("title", expect.stringMatching(/\S/));
    expect(bell).toHaveTextContent(/^Needs you$/);

    // A card's own icons sit in the action group.
    const cardGroup = card(ROOT).getByTestId("action-icon-group");
    expect(within(cardGroup).getByRole("button", { name: "Merge it" })).toHaveAttribute("data-action", "card-merge");
    expect(within(cardGroup).queryByRole("button", { name: /^Close/ }), "Close stays outside the group").toBeNull();

    // Ticking a card opens nothing but shows the bar.
    fireEvent.click(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" }));
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();
    const bar = screen.getByTestId("agent-cards-selection-bar");
    expect(within(bar).getByTestId("agent-cards-selection-count")).toHaveTextContent("1 selected");
    const barGroup = within(bar).getByTestId("action-icon-group");
    expect(within(barGroup).getAllByRole("button").map((button) => button.getAttribute("aria-label"))).toEqual(["Merge it", "Go ahead"]);
    expect(within(barGroup).queryByRole("button", { name: "Clear" }), "Clear is not an action").toBeNull();

    // Clear empties the selection and takes the bar away.
    fireEvent.click(within(bar).getByRole("button", { name: "Clear" }));
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
    expect(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" })).not.toBeChecked();
    expect(calls.invokeCardActionBatch).not.toHaveBeenCalled();
    expect(calls.invokeCardAction).not.toHaveBeenCalled();

    cleanup();

    // A daemon without batches: no checkbox, no bar.
    setup({ batch: false });
    expect(card(ROOT).queryByRole("checkbox")).toBeNull();
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
  });

  /// Scenario: Start a batch whose action has confirm=false. Its confirmation quotes each daemon-previewed prompt and skipped reason without an editable text box; Cancel executes nothing, and a later Send submits precisely the previewed included tokens and instances.
  it("desktop/agent-cards/030: every batch reviews the daemon's exact text before Send", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    showSkipped(dialog);
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
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    await act(async () => { fireEvent.click(within(screen.getByTestId("agent-cards-batch-confirm")).getByRole("button", { name: /^Send$/ })); });
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(3);
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({
      mode: "execute", entries: INCLUDED.map(({ pane_id, agent_id, action_token, instance_id }) => ({ pane_id, agent_id, action_token, instance_id })),
    }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Send a reviewed batch in which the root's text is queued and the child's action has changed. The result names each agent, marks the root queued for the agent and the child failed with the daemon's reason, and counts one queued and one failed without claiming delivery.
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
    expect(within(root!).getByText("Queued for the agent", { exact: true })).toBeInTheDocument();
    expect(within(root!).queryByText("Delivered", { exact: true })).toBeNull();
    expect(within(child!).getByText("Failed", { exact: true })).toBeInTheDocument();
    expect(within(child!).getByText("The action changed after you reviewed it.")).toBeInTheDocument();
    expect(within(results).getByRole("status")).toHaveTextContent("1 queued for the agent of 2; 1 failed");
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Select one card on each of two daemons whose cards share the same pane and agent ids but have different names, and send a batch. The confirmation and results name each agent with its own daemon, and the local daemon's nameless failure is shown under the local agent's name, never the remote one's, with no duplicate rows.
  it("desktop/agent-cards/034: a batch across two daemons keeps each agent's daemon, even with the same ids", async () => {
    const errors = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const calls = setupTwoDaemons();
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    const rowFor = (scope: HTMLElement, name: string) => within(scope).getAllByTestId("agent-cards-batch-entry").filter((row) => within(row).queryByText(name, { exact: true }));
    expect(within(dialog).getAllByTestId("agent-cards-batch-entry")).toHaveLength(2);
    expect(within(rowFor(dialog, "local-coder")[0]).getByTestId("agent-cards-batch-daemon")).toHaveTextContent(`on ${calls.localName}`);
    expect(within(rowFor(dialog, "remote-coder")[0]).getByTestId("agent-cards-batch-daemon")).toHaveTextContent(`on ${calls.remoteName}`);
    expect(within(rowFor(dialog, "local-coder")[0]).getByTestId("agent-cards-batch-prompt").textContent).toBe("local-coder: go ahead.");

    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    expect(within(results).getAllByTestId("agent-cards-batch-entry")).toHaveLength(2);
    const failed = rowFor(results, "local-coder");
    const delivered = rowFor(results, "remote-coder");
    expect(failed).toHaveLength(1);
    expect(delivered).toHaveLength(1);
    expect(failed[0]).toHaveAttribute("data-outcome", "failed");
    expect(within(failed[0]).getByTestId("agent-cards-batch-daemon")).toHaveTextContent(`on ${calls.localName}`);
    expect(within(failed[0]).getByText("The action changed after you reviewed it.")).toBeInTheDocument();
    expect(delivered[0]).toHaveAttribute("data-outcome", "delivered");
    expect(within(delivered[0]).getByTestId("agent-cards-batch-daemon")).toHaveTextContent(`on ${calls.remoteName}`);
    expect(calls.invokeCardActionBatch).toHaveBeenCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "execute", instanceId: INSTANCE }));
    expect(calls.invokeCardActionBatch).toHaveBeenCalledWith(FIXTURE_REMOTE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "execute", instanceId: REMOTE_INSTANCE }));
    expect(errors.mock.calls.filter((call) => String(call[0]).includes("same key"))).toEqual([]);
    errors.mockRestore();
  });

  /// Scenario: Tick one card on each of two daemons, the local card also offering an action the remote one does not, then the local daemon stops advertising batches. The local card loses its checkbox and leaves the bar's count and its actions, and an action from the bar previews only the remote daemon's card. With only the local card ticked, its daemon losing batches takes the bar away and hands focus to the dashboard, and the card is not ticked again when batches come back.
  it("desktop/agent-cards/091: a daemon that stops doing batches takes its cards out of the selection bar", async () => {
    const localOnly: AgentCardAction = { ...action, id: "batch-local-only", label: "Local only", token: "tok-local-only" };
    const calls = setupTwoDaemons({ localActions: [action, localOnly] });
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    expect(selectionAction("Local only")).toBeInTheDocument();

    act(() => calls.setBatch(FIXTURE_DAEMON_ID, false));
    expect(screen.queryByRole("checkbox", { name: "Select local-coder" })).toBeNull();
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("1 selected");
    expect(within(screen.getByTestId("agent-cards-selection-bar")).queryByRole("button", { name: "Local only" })).toBeNull();
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toBeInTheDocument();
    const previewed = calls.invokeCardActionBatch.mock.calls.filter(([, , options]) => options.mode === "preview");
    expect(previewed.map(([deckId]) => deckId)).toEqual([FIXTURE_REMOTE_DAEMON_ID]);
    fireEvent.click(screen.getByTestId("agent-cards-batch-cancel"));

    // Only the local card ticked: its daemon losing batches takes the bar away, with focus handed to the dashboard.
    fireEvent.click(screen.getByTestId("agent-cards-selection-clear"));
    act(() => calls.setBatch(FIXTURE_DAEMON_ID, true));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("1 selected");
    act(() => { screen.getByTestId("agent-cards-selection-clear").focus(); });
    act(() => calls.setBatch(FIXTURE_DAEMON_ID, false));
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
    expect(document.activeElement, "focus is on the dashboard").toBe(document.querySelector(".overview-body"));
    // The card left the selection rather than being hidden from it: batches coming back do not tick it again.
    act(() => calls.setBatch(FIXTURE_DAEMON_ID, true));
    expect(screen.getByRole("checkbox", { name: "Select local-coder" })).not.toBeChecked();
    expect(screen.queryByTestId("agent-cards-selection-bar")).toBeNull();
  });

  /// Scenario: With a batch confirmation open and nothing being sent, press Escape. The dialog closes and nothing is sent, as Cancel does.
  it("desktop/agent-cards/037: Escape dismisses a batch confirmation that is not being sent", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(screen.queryByTestId("agent-cards-batch-confirm")).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(1);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Send a reviewed batch to a daemon that takes it and never answers, so the dialog stays in Sending and Escape is refused. When the desktop stops waiting, each agent's row says "No answer" with the sentence that the deck may still deliver it, never "Failed" or "Nothing was sent"; nothing is sent again, Escape closes the results, and a new batch can be started.
  it("desktop/agent-cards/038: a batch the daemon never answers ends as no answer and the dialog can be closed", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    let answer: (value: unknown) => void = () => undefined;
    calls.invokeCardActionBatch.mockImplementationOnce(() => new Promise((resolve) => { answer = resolve; }) as never);
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "sending");
    fireEvent.keyDown(screen.getByTestId("agent-cards-batch-confirm"), { key: "Escape" });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "sending");
    expect(screen.getByTestId("agent-cards-batch-refusal")).toBeInTheDocument();

    // What the desktop bridge answers for every entry it sent once its bound for the batch runs out.
    const NO_ANSWER = "the deck did not answer within 35s, so it is not known whether this agent got it; the deck may still deliver it. Check the agent before sending again";
    await act(async () => {
      answer({ request_id: "rid", entries: INCLUDED.map(({ pane_id, agent_id }) => ({ pane_id, agent_id, outcome: "unknown", code: "no_answer", message: NO_ANSWER })) });
    });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    expect(rows).toHaveLength(2);
    for (const name of ["root", "child"]) {
      const row = rows.find((each) => within(each).queryByText(name, { exact: true }));
      expect(row, name).toBeDefined();
      expect(row).toHaveAttribute("data-outcome", "unknown");
      expect(within(row!).getByText("No answer", { exact: true })).toBeInTheDocument();
      expect(within(row!).getByText(NO_ANSWER)).toBeInTheDocument();
      expect(within(row!).queryByText("Failed", { exact: true })).toBeNull();
    }
    expect(within(results).getByRole("status")).toHaveTextContent("0 of 2 confirmed sent; 2 not answered, may still be delivered");
    expect(within(results).queryByText(/Nothing was sent/)).toBeNull();
    expect(within(results).queryByText(/Sent to all/)).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);

    fireEvent.keyDown(results, { key: "Escape" });
    expect(screen.queryByTestId("agent-cards-batch-results")).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toHaveAttribute("data-stage", "review");
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "preview" }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Send a reviewed batch whose execution fails in the window with an error the desktop bridge could not place before or after the request left. Each agent's row says "No answer" with that error and that it is not known whether the agent got it, never "Failed" or "Nothing was sent", and nothing is sent again.
  it("desktop/agent-cards/039: an execution that fails unplaced ends as no answer, not as failed", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    calls.invokeCardActionBatch.mockImplementationOnce(() => Promise.reject(new Error("the window lost the bridge")) as never);
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(row).toHaveAttribute("data-outcome", "unknown");
      expect(within(row).getByText("No answer", { exact: true })).toBeInTheDocument();
      expect(within(row).getByText(`the window lost the bridge; ${BATCH_NOT_KNOWN}`)).toBeInTheDocument();
      expect(within(row).queryByText("Failed", { exact: true })).toBeNull();
    }
    expect(within(results).getByRole("status")).toHaveTextContent("0 of 2 confirmed sent; 2 not answered, may still be delivered");
    expect(within(results).queryByText(/[Nn]othing was sent/)).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });
  /// Scenario: Send a reviewed batch that the daemon answers with each text queued in its agent's inbox, as a single press is when it answers. Each agent's row reads "Queued for the agent", the same words the card's delivery history uses, and the headline says it was queued for all of them, never "Delivered" or "Sent to all".
  it("desktop/agent-cards/040: a queued batch reads as queued, as a single press does, never as delivered", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    calls.invokeCardActionBatch.mockImplementationOnce(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
      request_id: options.requestId,
      entries: INCLUDED.map((entry) => ({ ...entry, outcome: "delivered" as const, message_id: `m-${entry.agent_id}`, state: "queued" })),
    }));
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(row).toHaveAttribute("data-outcome", "delivered");
      expect(within(row).getByText("Queued for the agent", { exact: true })).toBeInTheDocument();
      expect(within(row).queryByText("Delivered", { exact: true })).toBeNull();
    }
    expect(within(results).getByRole("status")).toHaveTextContent("Queued for all 2");
    expect(within(results).queryByText(/Sent to all|Delivered/)).toBeNull();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(2);
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Select one card on each of two daemons and start a batch while one daemon cannot answer its preview. The confirmation still opens: the reachable daemon's agent is included with its exact text, the other daemon's agent is skipped with the reason and its daemon's error, and Send sends only to the daemon that previewed. When neither daemon can answer, the confirmation says nothing was sent, with the error, and offers no Send.
  it("desktop/agent-cards/041: one daemon whose preview fails does not block the others' agents", async () => {
    const calls = setupTwoDaemons();
    const answer = calls.invokeCardActionBatch.getMockImplementation()!;
    calls.invokeCardActionBatch.mockImplementation(async (deckId: string, actionId: string, options: BatchOptions) => {
      if (deckId === FIXTURE_DAEMON_ID && options.mode === "preview") throw new Error("the deck is not reachable");
      return answer(deckId, actionId, options);
    });
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    expect(dialog).toHaveAttribute("data-stage", "review");
    expect(within(dialog).queryByText(/Nothing was sent/)).toBeNull();
    showSkipped(dialog);
    const rowFor = (scope: HTMLElement, name: string) => within(scope).getAllByTestId("agent-cards-batch-entry").filter((row) => within(row).queryByText(name, { exact: true }));
    const included = rowFor(dialog, "remote-coder");
    expect(included).toHaveLength(1);
    expect(included[0]).toHaveClass("is-included");
    expect(within(included[0]).getByTestId("agent-cards-batch-prompt").textContent).toBe("remote-coder: go ahead.");
    const skipped = rowFor(dialog, "local-coder");
    expect(skipped).toHaveLength(1);
    expect(skipped[0]).toHaveClass("is-skipped");
    expect(within(skipped[0]).getByTestId("agent-cards-batch-daemon")).toHaveTextContent(`on ${calls.localName}`);
    expect(within(skipped[0]).getByText(`${BATCH_DAEMON_UNREACHABLE}: the deck is not reachable`)).toBeInTheDocument();
    expect(within(dialog).getByText("Sends to 1 agent")).toBeInTheDocument();

    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const executed = calls.invokeCardActionBatch.mock.calls.filter(([, , options]) => options.mode === "execute");
    expect(executed.map(([deckId]) => deckId)).toEqual([FIXTURE_REMOTE_DAEMON_ID]);
    const results = screen.getByTestId("agent-cards-batch-results");
    expect(within(results).getAllByTestId("agent-cards-batch-entry")).toHaveLength(1);
    expect(rowFor(results, "remote-coder")[0]).toHaveAttribute("data-outcome", "delivered");
    fireEvent.click(within(results).getByTestId("agent-cards-batch-close"));

    calls.invokeCardActionBatch.mockImplementation(async (deckId: string) => {
      throw new Error(deckId === FIXTURE_DAEMON_ID ? "the deck is not reachable" : "the far deck timed out");
    });
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    const failed = screen.getByTestId("agent-cards-batch-confirm");
    expect(within(failed).getByRole("alert")).toHaveTextContent("Nothing was sent: the deck is not reachable; the far deck timed out");
    expect(within(failed).queryAllByTestId("agent-cards-batch-entry")).toHaveLength(0);
    expect(within(failed).getByRole("button", { name: /^Send$/ })).toBeDisabled();
    expect(calls.invokeCardActionBatch.mock.calls.filter(([, , options]) => options.mode === "execute")).toHaveLength(1);
  });

  /// Scenario: Send a reviewed batch that a daemon answers with each text delivered but no delivery state, as an earlier development build of the batch verb did. Each agent's row reads "Queued for the agent", the most a press can say when it answers, and the headline says it was queued for all of them, never "Delivered" or "Sent to all".
  it("desktop/agent-cards/042: a delivered entry without a delivery state reads as queued, never as delivered", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    calls.invokeCardActionBatch.mockImplementationOnce(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
      request_id: options.requestId,
      entries: INCLUDED.map(({ pane_id, agent_id, name }) => ({ pane_id, agent_id, name, outcome: "delivered" as const, message_id: `m-${agent_id}` })),
    }));
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(row).toHaveAttribute("data-outcome", "delivered");
      expect(within(row).getByText("Queued for the agent", { exact: true })).toBeInTheDocument();
    }
    expect(within(results).getByRole("status")).toHaveTextContent("Queued for all 2");
    expect(within(results).queryByText(/Sent to all|Delivered/)).toBeNull();
  });

  /// Scenario: Select one card on each of two daemons and start a batch while one daemon refuses the preview whole, as it refuses a batch naming more cards than it allows. That daemon's agent is skipped with the daemon's own reason, not with the sentence for a daemon that did not answer, the other daemon's agent is included, and Send sends only to the daemon that previewed.
  it("desktop/agent-cards/043: a daemon that refuses the preview shows its reason, not that it did not answer", async () => {
    const calls = setupTwoDaemons();
    const answer = calls.invokeCardActionBatch.getMockImplementation()!;
    // What the desktop bridge answers for a preview the daemon refused whole.
    const REFUSED = "the deck refused the request: invoke-card-action-batch: a batch may name at most 256 cards; this one names more; nothing is sent to this agent";
    calls.invokeCardActionBatch.mockImplementation(async (deckId: string, actionId: string, options: BatchOptions) => {
      if (deckId === FIXTURE_DAEMON_ID && options.mode === "preview") {
        return { request_id: options.requestId, entries: options.entries.map(({ pane_id, agent_id }) => ({ pane_id, agent_id, outcome: "skipped" as const, code: "deck_refused", message: REFUSED })) };
      }
      return answer(deckId, actionId, options);
    });
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    expect(dialog).toHaveAttribute("data-stage", "review");
    showSkipped(dialog);
    const rowFor = (scope: HTMLElement, name: string) => within(scope).getAllByTestId("agent-cards-batch-entry").filter((row) => within(row).queryByText(name, { exact: true }));
    const skipped = rowFor(dialog, "local-coder");
    expect(skipped).toHaveLength(1);
    expect(skipped[0]).toHaveClass("is-skipped");
    expect(within(skipped[0]).getByText(REFUSED)).toBeInTheDocument();
    expect(within(dialog).queryByText(new RegExp(BATCH_DAEMON_UNREACHABLE))).toBeNull();
    expect(within(dialog).queryByText(/did not answer/)).toBeNull();
    expect(rowFor(dialog, "remote-coder")[0]).toHaveClass("is-included");

    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const executed = calls.invokeCardActionBatch.mock.calls.filter(([, , options]) => options.mode === "execute");
    expect(executed.map(([deckId]) => deckId)).toEqual([FIXTURE_REMOTE_DAEMON_ID]);
  });

  /// Scenario: Send two reviewed batches whose daemon answers one agent and leaves the other unanswered: first a prompt the daemon queued for one agent, then a close it confirmed for one agent. The headline counts each outcome in its own words, "1 queued for the agent of 2" and "1 closed of 2", says the unanswered agent may still be delivered or may still be closed as its action is, and never calls a closed agent sent.
  it("desktop/agent-cards/044: the results headline counts queued, closed, failed and unanswered agents apart", async () => {
    const calls = setup();
    const unanswered = (entry: BatchEntry) => ({ pane_id: entry.pane_id, agent_id: entry.agent_id, outcome: "unknown", code: "no_answer", message: "the deck did not answer within 35s" });
    let dialog = await startSelectedBatch();
    calls.invokeCardActionBatch.mockImplementationOnce(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
      request_id: options.requestId,
      entries: [{ ...INCLUDED[0], outcome: "delivered", message_id: "m-root", state: "queued" }, unanswered(INCLUDED[1])],
    }) as never);
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    let results = screen.getByTestId("agent-cards-batch-results");
    expect(within(results).getByRole("status")).toHaveTextContent("1 queued for the agent of 2; 1 not answered, may still be delivered");
    expect(within(results).queryByText(/confirmed sent/)).toBeNull();
    fireEvent.click(within(results).getByTestId("agent-cards-batch-close"));

    const third = { pane_id: OUTSIDE.pane_id, agent_id: OUTSIDE.agent_id, name: OUTSIDE.name };
    const closing = [...INCLUDED.map(({ prompt: _prompt, ...entry }) => entry), { ...third, outcome: "included" as const, action_token: "tok-preview-outside", instance_id: INSTANCE }]
      .map((entry) => ({ ...entry, kind: "close", label: "Close" }));
    calls.invokeCardActionBatch.mockImplementation(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
      request_id: options.requestId,
      entries: options.mode === "preview"
        ? [...closing, SKIPPED]
        : [{ ...INCLUDED[0], outcome: "closed" }, { ...INCLUDED[1], outcome: "failed", code: "close_failed", message: "the agent did not stop" }, { ...third, outcome: "unknown", code: "no_answer", message: "the deck did not answer within 45s" }],
    }) as never);
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    dialog = screen.getByTestId("agent-cards-batch-confirm");
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    results = screen.getByTestId("agent-cards-batch-results");
    const headline = within(results).getByRole("status");
    expect(headline).toHaveTextContent("1 closed of 3; 1 failed; 1 not answered, may still be closed");
    expect(headline).not.toHaveTextContent(/sent|delivered/i);
  });

  /// Scenario: Send a reviewed batch to two agents whose daemon posts to the first and is then interrupted before it can say what became of the second, so it answers the second `unknown` with code `interrupted`. The first agent's row reads "Queued for the agent"; the second reads "No answer" with the daemon's sentence, never "Failed" and never that nothing was delivered.
  it("desktop/agent-cards/045: an agent the daemon cannot account for after starting a batch reads No answer, never Failed", async () => {
    const calls = setup();
    const dialog = await startSelectedBatch();
    const INTERRUPTED = "the deck was interrupted while running this batch, so it is not known whether this agent got it; check its card's delivery history before sending again";
    calls.invokeCardActionBatch.mockImplementationOnce(async (_deckId: string, _actionId: string, options: BatchOptions) => ({
      request_id: options.requestId,
      entries: [
        { ...INCLUDED[0], outcome: "delivered", message_id: "m-root", state: "queued" },
        { pane_id: INCLUDED[1].pane_id, agent_id: INCLUDED[1].agent_id, outcome: "unknown", code: "interrupted", message: INTERRUPTED },
      ],
    }) as never);
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    const results = screen.getByTestId("agent-cards-batch-results");
    const rows = within(results).getAllByTestId("agent-cards-batch-entry");
    expect(rows.map((row) => row.getAttribute("data-outcome"))).toEqual(["delivered", "unknown"]);
    expect(within(rows[0]).getByText("Queued for the agent", { exact: true })).toBeInTheDocument();
    expect(within(rows[1]).getByText("No answer", { exact: true })).toBeInTheDocument();
    expect(within(rows[1]).getByText(INTERRUPTED)).toBeInTheDocument();
    expect(within(results).queryByText(/^Failed$/)).toBeNull();
    expect(results).not.toHaveTextContent(/nothing was delivered|nothing was sent/);
    expect(within(results).getByRole("status")).not.toHaveTextContent(/failed/);
  });

  /// Scenario: Select a closed root whose three cards all list Go ahead with the same text. The selection toolbar's Go ahead is an icon button named Go ahead, with no text, whose hover says the label and that exact text; with a child whose Go ahead sends other words, its hover says the text differs per agent instead. An action with the catalog's `git-merge` icon draws it, the same drawing as on a card, and one with no icon or an icon the app does not bundle draws the generic icon. Pressing an icon still only opens the confirmation.
  it("desktop/agent-cards/051: the selection toolbar's batch buttons are icons with the label and text on hover", async () => {
    const same = action.prompt!;
    const expectIcon = (button: HTMLElement, icon = "generic") => {
      expect(button.textContent).toBe("");
      expect(button.querySelector("svg")).not.toBeNull();
      expect(button).toHaveAttribute("data-icon", icon);
      expect(button.getAttribute("data-tip")).toContain("Go ahead");
    };
    const calls = setup();
    selectRoot();
    const toolbar = selectionAction("Go ahead");
    expectIcon(toolbar);
    expect(toolbar.getAttribute("data-tip")).toContain(same);
    expect(toolbar.getAttribute("data-tip")).not.toMatch(/differs/);
    await act(async () => { fireEvent.click(toolbar); });
    expect(screen.getByTestId("agent-cards-batch-confirm")).toBeInTheDocument();
    expect(calls.invokeCardActionBatch).toHaveBeenCalledTimes(1);
    fireEvent.click(within(screen.getByTestId("agent-cards-batch-confirm")).getByRole("button", { name: /^Cancel$/ }));
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
    cleanup();

    // A child whose Go ahead sends other words: both surfaces' hover says so rather than quoting one agent's text.
    const differing = { ...CHILD, actions: [{ ...action, prompt: "Other words for the child." }] };
    const mixedCalls = setup({ entries: [ROOT, differing, STOPPED, OUTSIDE] });
    selectRoot();
    const mixed = selectionAction("Go ahead");
    expectIcon(mixed);
    expect(mixed.getAttribute("data-tip")).toMatch(/text differs per agent/);
    expect(mixed.getAttribute("data-tip")).not.toContain(same);
    expect(mixed.getAttribute("data-tip")).not.toContain("Other words for the child.");
    expect(mixedCalls.invokeCardAction).not.toHaveBeenCalled();
    expect(mixedCalls.invokeCardActionBatch).not.toHaveBeenCalled();
    cleanup();

    // The icon rule on both surfaces: a known catalog icon draws itself; no icon, or one the app does not bundle, draws the generic icon.
    const svg = (button: HTMLElement) => button.querySelector("svg")?.outerHTML;
    const drawn: Record<string, string | undefined> = {};
    for (const [icon, shown] of [["git-merge", "git-merge"], ["no-such-icon-2399", "generic"], [null, "generic"]] as const) {
      const withIcon = (one: AgentCardEntry): AgentCardEntry => ({ ...one, actions: one.actions.map((offered) => ({ ...offered, icon })) });
      setup({ entries: ENTRIES.map(withIcon) });
      selectRoot();
      const onToolbar = selectionAction("Go ahead");
      expectIcon(onToolbar, shown);
      drawn[String(icon)] = svg(onToolbar);
      cleanup();
    }
    expect(drawn["no-such-icon-2399"]).toBe(drawn["null"]);
    expect(drawn["git-merge"]).not.toBe(drawn["null"]);
  });

  /// Scenario: Start a batch whose preview gives the root and the child the same text, then one that also includes an unrelated agent with other text. When every included agent gets the same text, the confirmation quotes it exactly once and then lists the agents that get it; otherwise every agent is shown with its own exact text, unambiguously, whether quoted per agent or once per group of agents sharing it. Skipped agents keep their reason, and Send still sends exactly the previewed entries.
  it("desktop/agent-cards/052: the confirmation quotes text every agent shares once, with the list of agents", async () => {
    const SAME = "SAME-EXACT-2399: proceed.\nThen report.";
    const OTHER = "OTHER-EXACT-2399: wait for review.";
    const shared: BatchEntry[] = INCLUDED.map((entry) => ({ ...entry, prompt: SAME }));
    let calls = setup({ preview: [...shared, SKIPPED] });
    let dialog = await startSelectedBatch();
    const prompts = within(dialog).getAllByTestId("agent-cards-batch-prompt");
    expect(prompts.map((node) => node.textContent)).toEqual([SAME]);
    for (const name of ["root", "child"]) expect(within(dialog).getByText(name, { exact: true })).toBeInTheDocument();
    showSkipped(dialog);
    expect(within(dialog).getByText(SKIPPED.message!)).toBeInTheDocument();
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({
      mode: "execute", entries: shared.map(({ pane_id, agent_id, action_token, instance_id }) => ({ pane_id, agent_id, action_token, instance_id })),
    }));
    cleanup();

    // Two texts: every agent is shown with its own exact text, unambiguously — a quotation per agent, or one per
    // group of agents sharing a text; neither the order nor how the quotations are grouped is pinned.
    const outside: BatchEntry = { pane_id: OUTSIDE.pane_id, agent_id: OUTSIDE.agent_id, name: OUTSIDE.name, outcome: "included", prompt: OTHER, action_token: "tok-preview-outside", instance_id: INSTANCE };
    calls = setup({ preview: [shared[0], outside, shared[1], SKIPPED] });
    selectRoot();
    fireEvent.click(card(OUTSIDE).getByRole("checkbox", { name: "Select outside" }));
    await act(async () => { fireEvent.click(selectionAction("Go ahead")); });
    dialog = screen.getByTestId("agent-cards-batch-confirm");
    const quoted = within(dialog).getAllByTestId("agent-cards-batch-prompt").map((node) => node.textContent);
    expect(new Set(quoted)).toEqual(new Set([SAME, OTHER]));
    expect(quoted.length, "at most one quotation per included agent").toBeLessThanOrEqual(3);
    /* The quotation an agent's name is shown with: the nearest enclosing block that holds any quotation must hold exactly one text. */
    const quotationOf = (name: string) => {
      let node: HTMLElement | null = within(dialog).getByText(name, { exact: true });
      while (node && node !== dialog.parentElement) {
        const texts = new Set(within(node).queryAllByTestId("agent-cards-batch-prompt").map((found) => found.textContent));
        if (texts.size > 0) return texts;
        node = node.parentElement;
      }
      return new Set<string | null>();
    };
    for (const [name, text] of [["root", SAME], ["child", SAME], ["outside", OTHER]] as const) {
      expect(quotationOf(name), `${name} is shown with exactly its own text`).toEqual(new Set([text]));
    }
    showSkipped(dialog);
    expect(within(dialog).getByText(SKIPPED.message!)).toBeInTheDocument();
    expect(calls.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Start a batch whose preview skips 200 agents for two reasons, 150 that do not offer Go ahead and 50 that are not running. The confirmation shows one line, "200 skipped: 150 don't offer Go ahead, 50 aren't running", as a collapsed disclosure button; pressing it expands the full list grouped by reason, every name listed, and pressing it again collapses it.
  it("desktop/agent-cards/092: hundreds of skipped agents are one collapsed line, counted by reason, that expands grouped by reason", async () => {
    const offerless: BatchEntry[] = Array.from({ length: 150 }, (_, at) => ({ pane_id: `pane-x${at}`, agent_id: `x${at}`, name: `nogo-${at}`, outcome: "skipped", code: "unknown_action", message: `nogo-${at}'s card does not offer this action` }));
    const stopped: BatchEntry[] = Array.from({ length: 50 }, (_, at) => ({ pane_id: `pane-s${at}`, agent_id: `s${at}`, name: `halted-${at}`, outcome: "skipped", code: "agent_stopped", message: "the agent is not running" }));
    setup({ preview: [...INCLUDED, ...offerless, ...stopped] });
    const dialog = await startSelectedBatch();
    const toggle = within(dialog).getByRole("button", { name: "200 skipped: 150 don't offer Go ahead, 50 aren't running" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(dialog).queryByText("nogo-0", { exact: true })).toBeNull();
    expect(within(dialog).queryByText("halted-49", { exact: true })).toBeNull();
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    const groups = within(dialog).getAllByTestId("agent-cards-batch-skipped-group");
    expect(groups.map((group) => group.getAttribute("data-reason"))).toEqual(["unknown_action", "agent_stopped"]);
    expect(within(groups[0]).getByRole("heading", { name: "150 don't offer Go ahead" })).toBeInTheDocument();
    expect(within(groups[1]).getByRole("heading", { name: "50 aren't running" })).toBeInTheDocument();
    expect(within(groups[0]).getAllByTestId("agent-cards-batch-entry")).toHaveLength(150);
    expect(within(groups[1]).getAllByTestId("agent-cards-batch-entry")).toHaveLength(50);
    expect(within(groups[1]).getByText("halted-49", { exact: true })).toBeInTheDocument();
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(within(dialog).queryByTestId("agent-cards-batch-skipped-group")).toBeNull();
  });

  /// Scenario: Start a batch whose preview includes 30 agents that all get the same text. The text is quoted once, the first names are listed with a "+N more" button collapsed under them, and pressing it lists all 30 names; Send still sends to all 30.
  it("desktop/agent-cards/093: a long included list shows the first names and a +N more that expands, and Send still reaches every agent", async () => {
    const SAME = "SAME-MANY-2399: proceed.";
    const many: BatchEntry[] = Array.from({ length: 30 }, (_, at) => ({ pane_id: `pane-m${at}`, agent_id: `m${at}`, name: `many-${at}`, outcome: "included", prompt: SAME, action_token: `tok-m${at}`, instance_id: INSTANCE }));
    const calls = setup({ preview: many });
    const dialog = await startSelectedBatch();
    expect(within(dialog).getAllByTestId("agent-cards-batch-prompt").map((node) => node.textContent)).toEqual([SAME]);
    const shown = within(dialog).getAllByTestId("agent-cards-batch-entry");
    expect(shown.length).toBeLessThanOrEqual(10);
    expect(shown.length).toBeGreaterThan(0);
    const more = within(dialog).getByRole("button", { name: `+${30 - shown.length} more` });
    expect(more).toHaveAttribute("aria-expanded", "false");
    expect(within(dialog).queryByText("many-29", { exact: true })).toBeNull();
    fireEvent.click(more);
    expect(within(dialog).getAllByTestId("agent-cards-batch-entry")).toHaveLength(30);
    expect(within(dialog).getByText("many-29", { exact: true })).toBeInTheDocument();
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "execute", entries: many.map(({ pane_id, agent_id, action_token, instance_id }) => ({ pane_id, agent_id, action_token, instance_id })) }));
  });

  /// Scenario: Start a batch whose preview includes 11 agents in three text groups: nine get the catalog's text, and two get texts their agents wrote, each different. Before Send, every one of the three texts is quoted, each with its source mark, and the heading counts 11 agents and 3 different texts; Send sends to exactly the 11 agents shown. With thirty agents on the catalog's text instead, only that group's names fold behind "+22 more", and the two agent-written texts and their agents stay on screen.
  it("desktop/agent-cards/102: folding a long batch hides names, never a distinct text", async () => {
    const SHARED = "SHARED-102: go ahead.";
    const group = (count: number): BatchEntry[] => Array.from({ length: count }, (_, at) => ({ pane_id: `pane-k${at}`, agent_id: `k${at}`, name: `kept-${at}`, outcome: "included", prompt: SHARED, prompt_source: "catalog", action_token: `tok-k${at}`, instance_id: INSTANCE }));
    const odd: BatchEntry[] = [
      { pane_id: "pane-o1", agent_id: "o1", name: "odd-one", outcome: "included", prompt: "AGENT-102-A: also delete the scratch branch.", prompt_source: "agent", action_token: "tok-o1", instance_id: INSTANCE },
      { pane_id: "pane-o2", agent_id: "o2", name: "odd-two", outcome: "included", prompt: "AGENT-102-B: push to main directly.", prompt_source: "agent", action_token: "tok-o2", instance_id: INSTANCE },
    ];
    const texts = [SHARED, odd[0].prompt, odd[1].prompt];
    const sourceOf = (dialog: HTMLElement, text: string | null | undefined) => {
      const quote = within(dialog).getAllByTestId("agent-cards-batch-prompt").find((node) => node.textContent === text)!;
      return quote.closest("[data-testid='agent-cards-batch-group']")!.querySelector("[data-testid='agent-cards-batch-source']")?.textContent;
    };

    const eleven = [...group(9), ...odd];
    const calls = setup({ preview: eleven });
    let dialog = await startSelectedBatch();
    expect(within(dialog).getAllByTestId("agent-cards-batch-prompt").map((node) => node.textContent)).toEqual(texts);
    expect(sourceOf(dialog, SHARED)).toBe("catalog");
    expect(sourceOf(dialog, odd[0].prompt)).toBe("agent says");
    expect(sourceOf(dialog, odd[1].prompt)).toBe("agent says");
    expect(within(dialog).getByRole("heading", { level: 3 })).toHaveTextContent("Sends to 11 agents: 3 different texts");
    for (const name of ["odd-one", "odd-two", "kept-0", "kept-8"]) expect(within(dialog).getByText(name, { exact: true })).toBeInTheDocument();
    expect(within(dialog).getAllByTestId("agent-cards-batch-entry")).toHaveLength(11);
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: /^Send$/ })); });
    expect(calls.invokeCardActionBatch).toHaveBeenLastCalledWith(FIXTURE_DAEMON_ID, "batch-go", expect.objectContaining({ mode: "execute", entries: eleven.map(({ pane_id, agent_id, action_token, instance_id }) => ({ pane_id, agent_id, action_token, instance_id })) }));
    cleanup();

    setup({ preview: [...group(30), ...odd] });
    dialog = await startSelectedBatch();
    expect(within(dialog).getAllByTestId("agent-cards-batch-prompt").map((node) => node.textContent)).toEqual(texts);
    expect(within(dialog).getByRole("heading", { level: 3 })).toHaveTextContent("Sends to 32 agents: 3 different texts");
    for (const name of ["odd-one", "odd-two", "kept-7"]) expect(within(dialog).getByText(name, { exact: true })).toBeInTheDocument();
    expect(within(dialog).queryByText("kept-8", { exact: true })).toBeNull();
    const more = within(dialog).getByRole("button", { name: "+22 more" });
    fireEvent.click(more);
    expect(within(dialog).getByText("kept-29", { exact: true })).toBeInTheDocument();
    expect(within(dialog).getAllByTestId("agent-cards-batch-entry")).toHaveLength(32);
  });

  /// Scenario: Start a batch whose preview gives the root and the child the same exact text, the root's written by the catalog and the child's by its agent. The confirmation quotes the text once, then lists both agents, and the child's row still carries the badge saying its agent wrote the text (issue #2399 review 9).
  it("desktop/agent-cards/068: the same text from different sources is quoted once, each agent keeping its source badge", async () => {
    const SAME = "SAME-ANY-SOURCE-2399: proceed.";
    const preview: BatchEntry[] = [
      { ...INCLUDED[0], prompt: SAME, prompt_source: "catalog" },
      { ...INCLUDED[1], prompt: SAME, prompt_source: "agent" },
      SKIPPED,
    ];
    setup({ preview });
    const dialog = await startSelectedBatch();
    expect(within(dialog).getAllByTestId("agent-cards-batch-prompt").map((node) => node.textContent)).toEqual([SAME]);
    const entries = within(dialog).getAllByTestId("agent-cards-batch-entry");
    const child = entries.find((node) => within(node).queryByText("child", { exact: true }));
    const root = entries.find((node) => within(node).queryByText("root", { exact: true }));
    expect(child, "the child is listed").toBeDefined();
    expect(root, "the root is listed").toBeDefined();
    expect(within(child!).getByText("agent says")).toBeInTheDocument();
    expect(within(root!).queryByText("agent says")).toBeNull();
  });
});
