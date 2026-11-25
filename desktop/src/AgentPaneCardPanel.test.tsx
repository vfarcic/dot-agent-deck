import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { approvedProjectTrust } from "./test/projectTrust";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "./lib/bridge";
import { CARD_PANEL_STORAGE_KEY } from "./lib/paneCards";
import type { AgentCardAction, AgentCardEntry, DeckActionResult, DeckRuntimeState, EndedRole } from "./types";

/* The terminal is a focusable stand-in inside the pane's terminal box, so the
   keys can be pressed with the keyboard in it. */
vi.mock("./components/TerminalViewport", () => ({
  TerminalViewport: ({ agentId, label }: { agentId: string; label: string }) => (
    createElement("textarea", { "data-testid": `terminal-${agentId}`, "aria-label": `${label} terminal` })
  ),
}));

import { DeckShell } from "./App";
import { agentKey } from "./components/AgentOverview";

/*
 * PR #2491 decision 2026-11-25 — the agent pane's Card panel, on the real
 * shell: when it opens (from a card, for an agent that needs you, else as the
 * user last chose), that it never opens or closes by itself while the pane is
 * shown, and the header's moves between agents and their keys.
 */

const INSTANCE = "daemon-instance-pane-card";
const MODE_KEY = "dot-agent-deck.overview.mode";

function action(id: string, overrides: Partial<AgentCardAction> = {}): AgentCardAction {
  return {
    id, label: id, kind: "prompt", prompt: `Do ${id}.`, prompt_source: "catalog", text_input: false, confirm: false, confirm_detail: null, icon: null,
    placement: "card", custom: false, source: "catalog", enabled: true, disabled_reason: null, last_delivery: null, token: `tok-${id}`, ...overrides,
  };
}

function cardFor(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`, instance_id: INSTANCE, pane_id: `pane-${agentId}`, agent_id: agentId, agent_type: "claude_code", name,
    status: "working", needs_you: false, needs_you_reasons: [], group: { id: "standalone", kind: "standalone", label: null }, relations: {},
    report: { source: "agent", status: "working", summary: `${name} is working`, published_at: "2026-11-25T00:00:00Z", revision: 1 },
    fields: [], questions: [], actions: [action("go-ahead", { label: "Go ahead", prompt: "Go ahead." })],
    primary_action_id: "go-ahead", facts: { source: "deck", pull_request: null }, revision: 1, updated_at: "2026-11-25T00:00:00Z", ...overrides,
  };
}

function settingsStore() {
  let document: DesktopSettingsDto = { ...DEFAULT_DESKTOP_SETTINGS };
  return {
    getSettings: vi.fn(async () => ({ settings: structuredClone(document), path: undefined })),
    saveSettings: vi.fn(async (next: DesktopSettingsDto) => { document = structuredClone(next); return structuredClone(document); }),
  };
}

/**
 * How the deck looks in one render: connected (the default) or not answering,
 * extra fields of its snapshot, the agent ids it no longer lists (`gone`), and
 * whether its cards listing has not been read yet since it connected
 * (`unlisted`: connected, with no cards listing at all).
 */
type DeckState = { connected?: boolean; endedRoles?: EndedRole[]; tree?: boolean; gone?: string[]; unlisted?: boolean };

/** The real shell on the dashboard, with a card for each of the fixture deck's first three agents; the third needs you. */
function renderShell(initial: { extra?: AgentCardEntry[]; deck?: DeckState } = {}) {
  const base = createFixtureSnapshot("connected");
  const live = { ...base, connection: { ...base.connection, incarnation: INSTANCE }, agents: base.agents.map((agent) => ({ ...agent, incarnation: INSTANCE })) };
  const [first, second, third] = live.agents;
  const entries = [cardFor(first.id, "first-card"), cardFor(second.id, "second-card"), cardFor(third.id, "third-card", { needs_you: true, needs_you_reasons: ["question"] }), ...(initial.extra ?? [])];
  const settings = settingsStore();
  const resizeTerminal = vi.fn(async () => undefined);
  /* A deck that stops answering lists no agents and no cards, as the daemon's disconnected snapshot does. */
  const snapshotOf = (deck: DeckState) => (deck.connected === false
    ? { ...live, connection: { ...live.connection, status: "disconnected" as const, message: "The daemon stopped answering." }, agents: [], endedRoles: deck.endedRoles }
    : { ...live, agents: live.agents.filter((agent) => !deck.gone?.includes(agent.id)), endedRoles: deck.endedRoles });
  const invokeCardAction = vi.fn(async (_target: unknown, _actionId: string, options?: { requestId?: string }) => ({ request_id: options?.requestId ?? "r", outcome: "sent" as const, message_id: "m-pane-1" }));
  const runtime = (cards: AgentCardEntry[], revision: number, deck: DeckState = initial.deck ?? {}) => ({
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot: snapshotOf(deck),
    fleet: [snapshotOf(deck)],
    terminalData: {},
    clearError: vi.fn(),
    ...approvedProjectTrust(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal,
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    agentCards: deck.connected === false || deck.unlisted ? {} : { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision, entries: cards, catalogErrors: [], ...(deck.tree ? { tree: true } : {}) } },
    invokeCardAction,
  }) as unknown as DeckRuntimeState;
  const view = render(createElement(DeckShell, { runtime: runtime(entries, 1), initialView: { kind: "overview" } }));
  return {
    agents: [first, second, third],
    entries,
    resizeTerminal,
    invokeCardAction,
    update: (cards: AgentCardEntry[], revision: number, deck?: DeckState) => view.rerender(createElement(DeckShell, { runtime: runtime(cards, revision, deck), initialView: { kind: "overview" } })),
  };
}

const pane = () => screen.getByTestId("agent-pane-overlay");
const panel = () => within(pane()).queryByTestId("agent-card-panel");
const toggle = () => within(pane()).getByTestId("agent-pane-card-toggle");
const back = () => fireEvent.click(within(pane()).getByRole("button", { name: "Back to dashboard" }));
const row = (agentId: string) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: agentId })}`);
const openRow = (agentId: string) => fireEvent.click(row(agentId).querySelector("td:last-child") ?? row(agentId));
const card = (entry: AgentCardEntry) => document.querySelector<HTMLElement>(`article[data-card-key="${entry.key}"]`)!;

describe("the agent pane's Card panel (PR #2491 decision 2026-11-25)", () => {
  beforeEach(() => {
    window.history.replaceState({}, "", "/?fixture=1&experimental=1");
    window.localStorage.clear();
    vi.stubGlobal("matchMedia", vi.fn((query: string) => ({
      matches: false, media: query, onchange: null, addListener: vi.fn(), removeListener: vi.fn(),
      addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn(),
    })));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    window.localStorage.clear();
  });

  /// Scenario: Open an agent that does not need you from its table row: its pane opens with the Card panel closed. Press the header's Card button: the panel opens beside the terminal and the choice is remembered, so the next agent opened from a row shows it too; close it, and an agent opened from its card, or one that needs you, still opens with it. While a pane is shown, the agent's card changing — its summary, its status, coming to need you — never opens or closes the panel.
  it("desktop/agent-cards/095: the Card panel opens from a card or for an agent that needs you, else as last chosen, and never by itself", () => {
    const shell = renderShell();
    const [first, second, third] = shell.agents;
    openRow(first.id);
    expect(panel()).toBeNull();
    expect(toggle()).toHaveAttribute("aria-pressed", "false");
    fireEvent.click(toggle());
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    expect(within(panel()!).getByText("first-card is working")).toBeInTheDocument();
    expect(window.localStorage.getItem(CARD_PANEL_STORAGE_KEY)).toBe("open");
    // The panel sits beside the terminal, inside the pane's body.
    expect(panel()!.parentElement).toBe(within(pane()).getByTestId(`terminal-${first.id}`).closest(".agent-pane-body"));
    back();

    // The last choice: open.
    openRow(second.id);
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[1].key);
    fireEvent.click(toggle());
    expect(panel()).toBeNull();
    expect(window.localStorage.getItem(CARD_PANEL_STORAGE_KEY)).toBe("closed");
    // Updates while the pane is shown never open it, even when the agent comes to need you.
    shell.update([shell.entries[0], cardFor(second.id, "second-card", { needs_you: true, needs_you_reasons: ["question"], revision: 2, report: { source: "agent", status: "waiting", summary: "a question", published_at: "2026-11-25T00:01:00Z", revision: 2 } }), shell.entries[2]], 2);
    expect(panel()).toBeNull();
    back();

    // The last choice is closed, yet an agent that needs you opens with it.
    openRow(third.id);
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[2].key);
    // …and updates never close it either.
    shell.update([shell.entries[0], shell.entries[1], cardFor(third.id, "third-card", { needs_you: false, revision: 3 })], 3);
    expect(panel()).not.toBeNull();
    back();
    expect(window.localStorage.getItem(CARD_PANEL_STORAGE_KEY)).toBe("closed");

    // Opened from its card, an agent's pane shows the panel whatever was last chosen.
    fireEvent.click(screen.getByRole("radio", { name: "Cards" }));
    fireEvent.click(card(shell.entries[0]));
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    back();
    fireEvent.click(screen.getByRole("radio", { name: "Table" }));
    // A storage that throws still opens and closes the panel, for this pane.
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("denied"); });
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("denied"); });
    try {
      openRow(first.id);
      expect(panel()).toBeNull();
      fireEvent.click(toggle());
      expect(panel()).not.toBeNull();
    } finally {
      getItem.mockRestore();
      setItem.mockRestore();
    }
  });

  /// Scenario: With the Card panel last chosen open, open the first agent from its card and use the header: Next agent opens the second agent's pane, Previous agent the first's, Next needing you the third's, which needs you, each with its Card panel as the user chose. With the keyboard outside the terminal, `]`, `[` and `n` do the same; with the keyboard in the terminal, they are the user's typing and move nothing.
  it("desktop/agent-cards/096: the pane header moves between agents in the dashboard's order, and its keys stay out of the terminal", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    // The user's own choice is open, so each move shows that agent's card (J5: a move uses the choice, not the panel a card forced open).
    window.localStorage.setItem(CARD_PANEL_STORAGE_KEY, "open");
    const shell = renderShell();
    const [first, second, third] = shell.agents;
    fireEvent.click(card(shell.entries[0]));
    const shown = () => panel()?.getAttribute("data-card-key");
    expect(shown()).toBe(shell.entries[0].key);
    fireEvent.click(within(pane()).getByRole("button", { name: "Next agent" }));
    expect(shown()).toBe(shell.entries[1].key);
    expect(within(pane()).getByTestId(`terminal-${second.id}`)).toBeInTheDocument();
    fireEvent.click(within(pane()).getByRole("button", { name: "Previous agent" }));
    expect(shown()).toBe(shell.entries[0].key);
    fireEvent.click(within(pane()).getByRole("button", { name: /Next needing you/ }));
    expect(shown()).toBe(shell.entries[2].key);
    expect(within(pane()).getByTestId(`terminal-${third.id}`)).toBeInTheDocument();

    // The keys, with the keyboard on the pane but not in its terminal.
    act(() => { toggle().focus(); });
    fireEvent.keyDown(document.activeElement!, { key: "[" });
    expect(shown()).toBe(shell.entries[1].key);
    fireEvent.keyDown(document.activeElement!, { key: "]" });
    expect(shown()).toBe(shell.entries[2].key);
    fireEvent.keyDown(document.activeElement!, { key: "n" });
    expect(shown()).toBe(shell.entries[2].key);

    // In the terminal, the same keys move nothing: they are the agent's.
    const terminal = within(pane()).getByTestId(`terminal-${third.id}`);
    act(() => { terminal.focus(); });
    for (const key of ["[", "]", "n"]) fireEvent.keyDown(terminal, { key });
    expect(shown()).toBe(shell.entries[2].key);
    expect(within(pane()).getByTestId(`terminal-${third.id}`)).toBe(terminal);
    expect(first.id).not.toBe(third.id);
  });

  /// Scenario: With the Card panel last chosen closed, open the agent that needs you from its table row (its panel opens), then press Next agent to an agent that does not: its panel is closed, as chosen, not open because the needy one forced it. The same from a pane opened from its card: Next agent to an agent that does not need you shows it closed. Choosing open with the Card button, then moving, keeps it open.
  it("desktop/agent-cards/095: a move between agents uses the user's choice, not a panel the last agent forced open", () => {
    window.localStorage.setItem(CARD_PANEL_STORAGE_KEY, "closed");
    const shell = renderShell();
    const [first, second, third] = shell.agents;
    const next = () => fireEvent.click(within(pane()).getByRole("button", { name: "Next agent" }));
    const previous = () => fireEvent.click(within(pane()).getByRole("button", { name: "Previous agent" }));
    // The needy third agent forces its panel open; Next agent (wrapping) reaches the first, which does not need you.
    openRow(third.id);
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[2].key);
    next();
    expect(within(pane()).getByTestId(`terminal-${first.id}`)).toBeInTheDocument();
    expect(panel()).toBeNull();
    expect(toggle()).toHaveAttribute("aria-pressed", "false");
    back();
    // Opened from its card, the first agent's panel is open; Next agent to the second shows it closed.
    fireEvent.click(screen.getByRole("radio", { name: "Cards" }));
    fireEvent.click(card(shell.entries[0]));
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    next();
    expect(within(pane()).getByTestId(`terminal-${second.id}`)).toBeInTheDocument();
    expect(panel()).toBeNull();
    // The user's own choice travels: open it here, and the move back keeps it open.
    fireEvent.click(toggle());
    expect(window.localStorage.getItem(CARD_PANEL_STORAGE_KEY)).toBe("open");
    previous();
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
  });

  /// Scenario: A card the deck keeps for an orchestration role that has exited: its agent is not in the daemon's list of agents any more, its status is stopped, and its actions are disabled because the agent is not running. Click the card. A read-only pane opens: its header names the agent with its last status and offers no Close agent; the Card panel is open with why the role exited, its last output, its pull requests and issues and its report, and offers none of its actions; the terminal area says in one sentence that the agent has ended and its terminal is gone, and no terminal is attached. Next agent moves on from it, and Previous agent comes back. Its daemon then stops answering and answers again still keeping the card: the pane stays.
  it("desktop/agent-cards/104: a kept card whose agent is gone opens a read-only pane with its Card panel", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const goneId = "gone-104";
    const ended = cardFor(goneId, "ended-role", {
      status: "stopped",
      report: { source: "agent", status: "failed", summary: "Stopped after the tests failed", published_at: "2026-11-25T00:00:00Z", revision: 2 },
      actions: [action("go-ahead", { label: "Go ahead", prompt: "Go ahead.", enabled: false, disabled_reason: "agent_stopped" })],
      checked_refs: [{ field: "pull-request", kind: "pull_request", claim: "#24180", repo: "vfarcic/dot-agent-deck", number: 24180, url: "https://github.com/vfarcic/dot-agent-deck/pull/24180", result: { status: "checked", state: "open", checked_at: "2026-11-25T00:00:00Z", stale: false } }],
    } as Partial<AgentCardEntry>);
    const endedRoles: EndedRole[] = [{ paneId: ended.pane_id, agentId: goneId, roleIndex: 1, roleName: "coder", orchestrationName: "build", reason: "exited with status 1", headline: "coder exited", lastOutput: ["error: 3 tests failed"] }];
    const shell = renderShell({ extra: [ended], deck: { endedRoles, tree: true } });
    expect(shell.agents.some((agent) => agent.id === goneId)).toBe(false);
    fireEvent.click(card(ended));
    const shown = pane();
    expect(within(shown).getByRole("heading", { name: "ended-role" })).toBeInTheDocument();
    expect(within(shown).getByTestId("ended-pane-status")).toHaveTextContent(/^last seen: stopped$/i);
    expect(within(shown).queryByRole("button", { name: /Close agent/ })).toBeNull();
    expect(within(shown).queryByTestId(`terminal-${goneId}`)).toBeNull();
    expect(within(shown).getByTestId("terminal-ended")).toHaveTextContent("ended-role has ended, and its terminal is gone.");
    expect(shown.querySelector("[data-terminal-state]")?.getAttribute("data-terminal-state")).toBe("ended");
    const opened = panel()!;
    expect(opened.getAttribute("data-card-key")).toBe(ended.key);
    expect(within(opened).getByTestId("agent-card-panel-ended")).toHaveTextContent("exited with status 1");
    expect(within(opened).getByTestId("agent-card-panel-ended")).toHaveTextContent("error: 3 tests failed");
    expect(within(opened).getAllByTestId("agent-card-panel-ref")).toHaveLength(1);
    expect(within(opened).getByText("Stopped after the tests failed")).toBeInTheDocument();
    // Its actions need a live agent: the read-only pane offers none of them, and says so (audit M1).
    expect(within(opened).queryByRole("button", { name: "Go ahead" })).toBeNull();
    expect(within(opened).getByTestId("agent-card-panel-read-only")).toHaveTextContent("ended-role has ended, so its actions and answers are not available.");
    // The moves still work from it.
    fireEvent.click(within(shown).getByRole("button", { name: "Next agent" }));
    expect(within(pane()).getByTestId(`terminal-${shell.agents[0].id}`)).toBeInTheDocument();
    fireEvent.click(within(pane()).getByRole("button", { name: "Previous agent" }));
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();

    // Its daemon stops answering, then answers again still keeping the card: the pane stays, and shows the card again (review L1).
    shell.update(shell.entries, 3, { connected: false, endedRoles, tree: true });
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();
    expect(within(panel()!).getByTestId("agent-card-gone")).toBeInTheDocument();
    shell.update(shell.entries, 4, { endedRoles, tree: true });
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();
    expect(panel()?.getAttribute("data-card-key")).toBe(ended.key);
  });

  /// Scenario: Open the read-only pane of a card the deck keeps for an agent that has gone, then let its daemon stop answering and answer again. While the reconnected daemon has not listed its cards yet, the pane stays and says the card is not available; once it lists its cards without that one, the pane closes and the dashboard is back.
  it("desktop/agent-cards/104: a kept card's pane closes once its reconnected daemon lists its cards without it", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const goneId = "gone-104-retire";
    const ended = cardFor(goneId, "ended-role", { status: "stopped", actions: [action("go-ahead", { label: "Go ahead", enabled: false, disabled_reason: "agent_stopped" })] });
    const shell = renderShell({ extra: [ended] });
    fireEvent.click(card(ended));
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();
    shell.update(shell.entries, 2, { connected: false });
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();
    // Answering again, before its cards are listed: the agents' list says nothing about the cards, so the pane waits.
    shell.update(shell.entries, 3, { unlisted: true });
    expect(within(pane()).getByTestId("terminal-ended")).toBeInTheDocument();
    expect(within(panel()!).getByTestId("agent-card-gone")).toBeInTheDocument();
    // Its cards listed, without this one: the pane goes.
    shell.update(shell.entries.filter((entry) => entry.key !== ended.key), 4);
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.queryByTestId("ended-card-pane")).toBeNull();
  });

  /// Scenario: A card the deck keeps for an agent that is gone still lists an enabled action and a question with an answer button, as a card can for a moment while the deck catches up. Open it: the read-only pane shows the card's report and question but no action icon and no answer button, says its actions need a running agent, and nothing is ever sent.
  it("desktop/agent-cards/107: the read-only pane of an ended agent offers none of its card's actions or answers", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const goneId = "gone-107";
    const ended = cardFor(goneId, "ended-107", {
      status: "stopped",
      actions: [action("go-ahead", { label: "Go ahead", prompt: "EXACT-107 go ahead.", enabled: true }), action("answer-yes", { label: "Yes", prompt: "Yes.", placement: "question", enabled: true })],
      questions: [{ id: "q-107", text: "Ship it?", source: "agent", options: [{ id: "yes", label: "Yes", action_id: "answer-yes" }], answered: null }],
    } as Partial<AgentCardEntry>);
    const shell = renderShell({ extra: [ended] });
    fireEvent.click(card(ended));
    const shown = screen.getByTestId("ended-card-pane");
    const opened = panel()!;
    expect(opened.getAttribute("data-card-key")).toBe(ended.key);
    expect(within(opened).getByText("Ship it?")).toBeInTheDocument();
    expect(within(opened).queryByTestId("agent-card-panel-actions")).toBeNull();
    expect(within(opened).queryAllByTestId("agent-card-option")).toHaveLength(0);
    expect(within(opened).queryByRole("button", { name: /Go ahead|Yes/ })).toBeNull();
    expect(within(opened).getByTestId("agent-card-panel-read-only")).toHaveTextContent("ended-107 has ended, so its actions and answers are not available.");
    // Every button the pane still offers moves or leaves; none of them reaches the agent.
    for (const button of within(shown).getAllByRole("button")) expect(button.getAttribute("aria-label") ?? button.textContent).toMatch(/Previous agent|Next agent|Next needing you|Back to dashboard/);
    expect(shell.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: Open a working agent's pane from its card. The agent goes while the deck keeps its stopped card, so the pane becomes the read-only ended pane. The daemon stops answering: the pane stays the ended pane, not the agent's earlier live pane. The daemon answers again, still without the agent: still the ended pane. When the deck lists the agent again, its live pane with its terminal is back.
  it("desktop/agent-cards/108: an agent that ended keeps its ended pane through a disconnect, until the deck lists it again", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    const [first] = shell.agents;
    fireEvent.click(card(shell.entries[0]));
    expect(within(pane()).getByTestId(`terminal-${first.id}`)).toBeInTheDocument();
    const stopped = cardFor(first.id, "first-card", { status: "stopped", revision: 2, actions: [action("go-ahead", { label: "Go ahead", enabled: false, disabled_reason: "agent_stopped" })] });
    const cards = [stopped, shell.entries[1], shell.entries[2]];
    shell.update(cards, 2, { gone: [first.id] });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    shell.update(cards, 3, { connected: false });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(within(pane()).queryByTestId(`terminal-${first.id}`)).toBeNull();
    expect(within(pane()).queryByText(/daemon is not answering/)).toBeNull();
    shell.update(cards, 4, { gone: [first.id] });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(panel()?.getAttribute("data-card-key")).toBe(stopped.key);
    // A fresh record of the same agent: its live pane again.
    shell.update(shell.entries, 5);
    expect(screen.queryByTestId("ended-card-pane")).toBeNull();
    expect(within(pane()).getByTestId(`terminal-${first.id}`)).toBeInTheDocument();
  });

  /// Scenario: Open the first agent from its card, so its Card panel is open, then let its daemon stop answering, and answer again. While it does not answer, the pane keeps the panel's place, which says the card is not available right now and offers none of its actions, and the terminal is never resized; when the daemon answers again, the panel shows the card again in the same place.
  it("desktop/agent-cards/105: a disconnect keeps the open Card panel's place and says the card is not available", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    fireEvent.click(card(shell.entries[0]));
    const before = panel();
    expect(before?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    const resized = shell.resizeTerminal.mock.calls.length;
    shell.update(shell.entries, 2, { connected: false });
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(panel()).not.toBeNull();
    expect(within(panel()!).getByTestId("agent-card-gone")).toHaveTextContent("This card is not available right now.");
    expect(within(panel()!).queryByTestId("agent-card-panel-actions")).toBeNull();
    expect(within(panel()!).queryByRole("button")).toBeNull();
    expect(toggle()).toHaveAttribute("aria-pressed", "true");
    shell.update(shell.entries, 3);
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    expect(within(panel()!).getByText("first-card is working")).toBeInTheDocument();
    expect(shell.resizeTerminal.mock.calls.length).toBe(resized);
  });

  /// Scenario: An agent reports a `url`, a `pull_request` and an `issue` field, each an `https://` address it chose, and the user opens its pane from its card. The Card panel shows each address as plain text the user can select and copy, not as a link.
  it("desktop/agent-cards/103: an agent-written field value is plain text, never a link", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    const field = (name: string, type: string, value: string) => ({ name, label: name, description: null, type, icon: null, placement: "detail", custom: false, source: "agent", value });
    const withFields = cardFor(shell.agents[0].id, "first-card", {
      revision: 2,
      fields: [
        field("links", "url", "https://agent-controlled.invalid/"),
        field("pull-request", "pull_request", "https://github.com/o/r/pull/7"),
        field("issue", "issue", "https://github.com/o/r/issues/3"),
      ],
    } as Partial<AgentCardEntry>);
    shell.update([withFields, shell.entries[1], shell.entries[2]], 2);
    fireEvent.click(card(withFields));
    const fields = panel()!.querySelector<HTMLElement>("dl[aria-label='Fields']")!;
    for (const value of ["https://agent-controlled.invalid/", "https://github.com/o/r/pull/7", "https://github.com/o/r/issues/3"]) {
      expect(within(fields).getByText(value).closest("a")).toBeNull();
    }
    expect(fields.querySelector("a")).toBeNull();
    expect(panel()!.querySelector("a[href='https://agent-controlled.invalid/']")).toBeNull();
  });
});
