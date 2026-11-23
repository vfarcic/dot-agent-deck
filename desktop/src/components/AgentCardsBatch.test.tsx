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

/*
 * Issue #2399 review 1 / audit S1 — two daemons whose cards have the SAME pane
 * and agent ids (they are daemon-local) and different names. Execution fails
 * on the local daemon with no `name`, the daemon's normal failure shape.
 */
const REMOTE_INSTANCE = "daemon-instance-batch-remote";
const SHARED = { pane_id: "pane-shared", agent_id: "shared" };

function setupTwoDaemons() {
  const [local, remote] = createFixtureFleet("fleet");
  const fleet = [
    { ...local, connection: { ...local.connection, incarnation: INSTANCE } },
    { ...remote, connection: { ...remote.connection, incarnation: REMOTE_INSTANCE } },
  ];
  const localCard = entry("shared", "local-coder", { key: "pane-shared/shared", pane_id: SHARED.pane_id });
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
          : { ...SHARED, name, outcome: "delivered", message_id: "m-remote" }],
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
  render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
  return { invokeCardActionBatch, localName: deckName(fleet[0].connection), remoteName: deckName(fleet[1].connection) };
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

  /// Scenario: Select one card on each of two daemons whose cards share the same pane and agent ids but have different names, and send a batch. The confirmation and results name each agent with its own daemon, and the local daemon's nameless failure is shown under the local agent's name, never the remote one's, with no duplicate rows.
  it("desktop/agent-cards/034: a batch across two daemons keeps each agent's daemon, even with the same ids", async () => {
    const errors = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const calls = setupTwoDaemons();
    fireEvent.click(screen.getByTestId("agent-cards-select"));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    expect(screen.getByTestId("agent-cards-selection-count")).toHaveTextContent("2 selected");
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
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
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
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
    fireEvent.click(screen.getByTestId("agent-cards-select"));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select local-coder" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Select remote-coder" }));
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
    const dialog = screen.getByTestId("agent-cards-batch-confirm");
    expect(dialog).toHaveAttribute("data-stage", "review");
    expect(within(dialog).queryByText(/Nothing was sent/)).toBeNull();
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
    await act(async () => { fireEvent.click(screen.getByTestId("agent-cards-batch-action-batch-go")); });
    const failed = screen.getByTestId("agent-cards-batch-confirm");
    expect(within(failed).getByRole("alert")).toHaveTextContent("Nothing was sent: the deck is not reachable; the far deck timed out");
    expect(within(failed).queryAllByTestId("agent-cards-batch-entry")).toHaveLength(0);
    expect(within(failed).getByRole("button", { name: /^Send$/ })).toBeDisabled();
    expect(calls.invokeCardActionBatch.mock.calls.filter(([, , options]) => options.mode === "execute")).toHaveLength(1);
  });
});
