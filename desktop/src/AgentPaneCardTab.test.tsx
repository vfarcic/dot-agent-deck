import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { approvedProjectTrust } from "./test/projectTrust";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "./lib/bridge";
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
 * PR #2491 decision 2026-11-26 — the agent pane's Card tab, on the real shell:
 * every way into an agent opens on Terminal and the Card tab is never
 * remembered, the Card tab is offered without the experimental flag and
 * carries a bell while the agent needs you, switching tabs never resizes the
 * terminal, the ended pane opens on Card, and the header's moves between
 * agents and their keys.
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
type DeckState = { connected?: boolean; endedRoles?: EndedRole[]; tree?: boolean; gone?: string[]; unlisted?: boolean; catalogEdit?: boolean };

/** The real shell on the dashboard, with a card for each of the fixture deck's first three agents; the third needs you. */
function renderShell(initial: { extra?: AgentCardEntry[]; deck?: DeckState; experimental?: boolean } = {}) {
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
  const readCardCatalog = vi.fn(async () => ({ actions: [], errors: [], user_config: "/home/user/.config/dot-agent-deck/config.toml" }));
  const runtime = (cards: AgentCardEntry[], revision: number, deck: DeckState = initial.deck ?? {}) => ({
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures(initial.experimental === false ? "" : "?experimental=1"),
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
    agentCards: deck.connected === false || deck.unlisted ? {} : { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision, entries: cards, catalogErrors: [], ...(deck.tree ? { tree: true } : {}), ...(deck.catalogEdit ? { catalogEdit: true } : {}) } },
    invokeCardAction,
    readCardCatalog,
    editCardCatalog: vi.fn(),
  }) as unknown as DeckRuntimeState;
  const view = render(createElement(DeckShell, { runtime: runtime(entries, 1), initialView: { kind: "overview" } }));
  return {
    agents: [first, second, third],
    entries,
    resizeTerminal,
    invokeCardAction,
    readCardCatalog,
    update: (cards: AgentCardEntry[], revision: number, deck?: DeckState) => view.rerender(createElement(DeckShell, { runtime: runtime(cards, revision, deck), initialView: { kind: "overview" } })),
  };
}


const pane = () => screen.getByTestId("agent-pane-overlay");
/** The Card tab's content: present only while the Card tab is the one shown. */
const panel = () => within(pane()).queryByTestId("agent-card-panel");
const tabs = () => within(pane()).getAllByRole("tab");
const shownTab = () => within(pane()).getByRole("tab", { selected: true });
const cardTab = () => within(pane()).getByRole("tab", { name: /^Card/ });
const terminalTab = () => within(pane()).getByRole("tab", { name: "Terminal" });
const back = () => fireEvent.click(within(pane()).getByRole("button", { name: "Back to dashboard" }));
const next = () => fireEvent.click(within(pane()).getByRole("button", { name: "Next agent" }));
const previous = () => fireEvent.click(within(pane()).getByRole("button", { name: "Previous agent" }));
const nextNeedingYou = () => fireEvent.click(within(pane()).getByRole("button", { name: /Next needing you/ }));
const row = (agentId: string) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: agentId })}`);
const openRow = (agentId: string) => fireEvent.click(row(agentId).querySelector("td:last-child") ?? row(agentId));
const card = (entry: AgentCardEntry) => document.querySelector<HTMLElement>(`article[data-card-key="${entry.key}"]`)!;

/** The pane shows `agentId`'s terminal on its Terminal tab, and no card. */
function expectOnTerminal(agentId: string, how: string) {
  expect(shownTab(), how).toHaveAccessibleName("Terminal");
  expect(panel(), `${how}: no card is shown`).toBeNull();
  expect(within(pane()).getByTestId(`terminal-${agentId}`), `${how}: ${agentId}'s terminal`).toBeInTheDocument();
}

describe("the agent pane's Card tab (PR #2491 decision 2026-11-26)", () => {
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

  /// Scenario: Open agents every way the dashboard offers — a Table row, a row's Show card, a card's body, a card's name, the card of an agent that needs you — and move between them with the header's Previous agent, Next agent and Next needing you and with the `[`, `]` and `n` keys, switching to the Card tab before each move. Every pane opens on its Terminal tab, and the Card tab chosen last is never carried to the next opening, even of the same agent.
  it("desktop/agent-cards/124: every way into an agent opens on Terminal, and the Card tab is never remembered", () => {
    const shell = renderShell();
    const [first, second, third] = shell.agents;

    // A Table row; the Card tab chosen there is not kept for the next opening of the same agent.
    openRow(first.id);
    expectOnTerminal(first.id, "a Table row");
    fireEvent.click(cardTab());
    expect(shownTab()).toBe(cardTab());
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    back();
    openRow(first.id);
    expectOnTerminal(first.id, "the same row again");

    // The header's moves, each from the Card tab.
    fireEvent.click(cardTab());
    next();
    expectOnTerminal(second.id, "Next agent");
    fireEvent.click(cardTab());
    previous();
    expectOnTerminal(first.id, "Previous agent");
    fireEvent.click(cardTab());
    nextNeedingYou();
    expectOnTerminal(third.id, "Next needing you, to an agent that needs you");

    // Their keys, with the keyboard on the Card tab.
    for (const [key, landsOn] of [["[", second.id], ["n", third.id], ["]", first.id]] as const) {
      fireEvent.click(cardTab());
      act(() => { cardTab().focus(); });
      fireEvent.keyDown(document.activeElement!, { key });
      expectOnTerminal(landsOn, `the ${key} key`);
    }
    back();

    // A row's Show card.
    fireEvent.click(within(row(second.id)).getByTestId("overview-row-menu"));
    fireEvent.click(screen.getByTestId("overview-row-show-card"));
    expectOnTerminal(second.id, "a row's Show card");
    back();

    // The Cards view: a card's body, its name, and the card of an agent that needs you.
    fireEvent.click(screen.getByRole("radio", { name: "Cards" }));
    fireEvent.click(card(shell.entries[0]));
    expectOnTerminal(first.id, "a card's body");
    back();
    fireEvent.click(within(card(shell.entries[1])).getByTestId("agent-card-open-agent"));
    expectOnTerminal(second.id, "a card's name");
    back();
    fireEvent.click(card(shell.entries[2]));
    expectOnTerminal(third.id, "the card of an agent that needs you");
  });

  /// Scenario: In the Table view, open the row menu of an agent whose most needed action asks first, and press that action. The agent's pane opens on its Terminal tab with the action's review over it, showing the action's exact text, and nothing is sent.
  it("desktop/agent-cards/124: a row's primary action opens the pane on Terminal under its review, and sends nothing", () => {
    const shell = renderShell();
    const [first] = shell.agents;
    const asking = cardFor(first.id, "first-card", { revision: 2, actions: [action("ship", { label: "Ship it", prompt: "TAB-124: ship it.", confirm: true })], primary_action_id: "ship" });
    shell.update([asking, shell.entries[1], shell.entries[2]], 2);
    fireEvent.click(within(row(first.id)).getByTestId("overview-row-menu"));
    fireEvent.click(screen.getByTestId("overview-row-primary"));
    expectOnTerminal(first.id, "a row's primary action");
    expect(screen.getByTestId("agent-card-confirm-prompt")).toHaveTextContent("TAB-124: ship it.");
    expect(shell.invokeCardAction).not.toHaveBeenCalled();
  });

  /// Scenario: With the experimental flag off, open an agent from its row: its pane's tabs are Terminal and Card, and no Diff, Checks, Delegations or Artifacts; the Card tab shows the agent's card. With the flag on, the tabs are Terminal, Card, Diff, Checks, Delegations and Artifacts, in that order. On a daemon that has not listed any cards, the pane has no Card tab.
  it("desktop/agent-cards/125: the Card tab is offered without the experimental flag, and the other detail tabs stay hidden", () => {
    const names = () => tabs().map((tab) => tab.getAttribute("aria-label"));
    const off = renderShell({ experimental: false });
    openRow(off.agents[0].id);
    expect(names()).toEqual(["Terminal", "Card"]);
    fireEvent.click(cardTab());
    expect(panel()?.getAttribute("data-card-key")).toBe(off.entries[0].key);
    cleanup();

    const on = renderShell();
    openRow(on.agents[0].id);
    expect(names()).toEqual(["Terminal", "Card", "Diff", "Checks", "Delegations", "Artifacts"]);
    cleanup();

    const unlisted = renderShell({ experimental: false, deck: { unlisted: true } });
    openRow(unlisted.agents[0].id);
    expect(names()).toEqual(["Terminal"]);
  });

  /// Scenario: Open an agent that needs you: its Card tab shows a bell and is named "Card, needs you", while the pane stays on Terminal. Open one that does not: its Card tab has no bell and is named "Card". That agent then comes to need you while its pane is shown: the bell appears and the name changes, and the pane stays on Terminal; when it no longer needs you, the bell goes.
  it("desktop/agent-cards/126: the Card tab carries a bell named \"Card, needs you\" while the agent needs you", () => {
    const shell = renderShell();
    const [first, , third] = shell.agents;
    const bell = () => within(cardTab()).queryByTestId("card-tab-bell");
    openRow(third.id);
    expect(cardTab()).toHaveAccessibleName("Card, needs you");
    expect(bell()).not.toBeNull();
    expect(bell()!.tagName).toBe("EM");
    expectOnTerminal(third.id, "an agent that needs you");
    back();

    openRow(first.id);
    expect(cardTab()).toHaveAccessibleName("Card");
    expect(bell()).toBeNull();
    const needy = cardFor(first.id, "first-card", { revision: 2, needs_you: true, needs_you_reasons: ["question"] });
    shell.update([needy, shell.entries[1], shell.entries[2]], 2);
    expect(cardTab()).toHaveAccessibleName("Card, needs you");
    expect(bell()).not.toBeNull();
    expectOnTerminal(first.id, "after the agent came to need you");
    shell.update([cardFor(first.id, "first-card", { revision: 3 }), shell.entries[1], shell.entries[2]], 3);
    expect(cardTab()).toHaveAccessibleName("Card");
    expect(bell()).toBeNull();
  });

  /// Scenario: Open an agent's pane and switch between its Terminal and Card tabs several times, with its card changing while the Card tab is shown. The terminal stays the same mounted element throughout, in the same box, and the agent's terminal is never resized.
  it("desktop/agent-cards/127: switching between Terminal and Card keeps the terminal mounted and never resizes it", () => {
    const shell = renderShell();
    const [first] = shell.agents;
    openRow(first.id);
    const terminal = within(pane()).getByTestId(`terminal-${first.id}`);
    const stack = terminal.closest(".agent-terminal-stack");
    const resized = shell.resizeTerminal.mock.calls.length;
    for (let round = 0; round < 3; round += 1) {
      fireEvent.click(cardTab());
      expect(panel()).not.toBeNull();
      expect(within(pane()).getByTestId(`terminal-${first.id}`), "the terminal is still mounted under the Card tab").toBe(terminal);
      expect(terminal.closest(".agent-terminal-stack")).toBe(stack);
      shell.update([cardFor(first.id, "first-card", { revision: 10 + round, report: { source: "agent", status: "working", summary: `round ${round}`, published_at: "2026-11-26T00:00:00Z", revision: 10 + round } }), shell.entries[1], shell.entries[2]], 10 + round);
      expect(within(panel()!).getByText(`round ${round}`)).toBeInTheDocument();
      fireEvent.click(terminalTab());
      expect(panel()).toBeNull();
      expect(within(pane()).getByTestId(`terminal-${first.id}`)).toBe(terminal);
    }
    expect(shell.resizeTerminal.mock.calls.length).toBe(resized);
  });

  /// Scenario: A card the deck keeps for an orchestration role that has exited: its agent is not in the daemon's list of agents any more, its status is stopped, and its actions are disabled because the agent is not running. Click the card. A read-only pane opens on its Card tab, since it has no terminal: its header names the agent with its last status and offers no Close agent; the Card tab shows why the role exited, its last output, its pull requests and issues and its report, and offers none of its actions; its Terminal tab says in one sentence that the agent has ended and its terminal is gone, and no terminal is attached. Next agent moves on from it, to a live agent's pane on Terminal, and Previous agent comes back to the ended pane, again on Card. Its daemon then stops answering and answers again still keeping the card: the pane stays.
  it("desktop/agent-cards/104: a kept card whose agent is gone opens a read-only pane on its Card tab", () => {
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
    const shown = screen.getByTestId("ended-card-pane");
    expect(within(shown).getByRole("heading", { name: "ended-role" })).toBeInTheDocument();
    expect(within(shown).getByTestId("ended-pane-status")).toHaveTextContent(/^last seen: stopped$/i);
    expect(within(shown).queryByRole("button", { name: /Close agent/ })).toBeNull();
    expect(within(shown).queryByTestId(`terminal-${goneId}`)).toBeNull();
    // It opens on Card: it has no terminal to show.
    expect(shownTab()).toBe(cardTab());
    expect(tabs().map((tab) => tab.getAttribute("aria-label"))).toEqual(["Terminal", "Card"]);
    const opened = panel()!;
    expect(opened.getAttribute("data-card-key")).toBe(ended.key);
    expect(within(opened).getByTestId("agent-card-panel-ended")).toHaveTextContent("exited with status 1");
    expect(within(opened).getByTestId("agent-card-panel-ended")).toHaveTextContent("error: 3 tests failed");
    expect(within(opened).getAllByTestId("agent-card-panel-ref")).toHaveLength(1);
    expect(within(opened).getByText("Stopped after the tests failed")).toBeInTheDocument();
    // Its actions need a live agent: the read-only pane offers none of them, and says so (audit M1).
    expect(within(opened).queryByRole("button", { name: "Go ahead" })).toBeNull();
    expect(within(opened).getByTestId("agent-card-panel-read-only")).toHaveTextContent("ended-role has ended, so its actions and answers are not available.");
    // Its Terminal tab says why there is no terminal.
    fireEvent.click(terminalTab());
    expect(panel()).toBeNull();
    expect(within(shown).getByTestId("terminal-ended")).toHaveTextContent("ended-role has ended, and its terminal is gone.");
    expect(shown.querySelector("[data-terminal-state]")?.getAttribute("data-terminal-state")).toBe("ended");
    // The moves still work from it: a live agent opens on Terminal, and the ended pane on Card again.
    fireEvent.click(within(shown).getByRole("button", { name: "Next agent" }));
    expectOnTerminal(shell.agents[0].id, "Next agent from the ended pane");
    previous();
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(shownTab()).toBe(cardTab());

    // Its daemon stops answering, then answers again still keeping the card: the pane stays, and shows the card again (review L1).
    shell.update(shell.entries, 3, { connected: false, endedRoles, tree: true });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(within(panel()!).getByTestId("agent-card-gone")).toBeInTheDocument();
    shell.update(shell.entries, 4, { endedRoles, tree: true });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(panel()?.getAttribute("data-card-key")).toBe(ended.key);
  });

  /// Scenario: Open the read-only pane of a card the deck keeps for an agent that has gone, then let its daemon stop answering and answer again. While the reconnected daemon has not listed its cards yet, the pane stays and its Card tab says the card is not available; once it lists its cards without that one, the pane closes and the dashboard is back.
  it("desktop/agent-cards/104: a kept card's pane closes once its reconnected daemon lists its cards without it", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const goneId = "gone-104-retire";
    const ended = cardFor(goneId, "ended-role", { status: "stopped", actions: [action("go-ahead", { label: "Go ahead", enabled: false, disabled_reason: "agent_stopped" })] });
    const shell = renderShell({ extra: [ended] });
    fireEvent.click(card(ended));
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    shell.update(shell.entries, 2, { connected: false });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    // Answering again, before its cards are listed: the agents' list says nothing about the cards, so the pane waits.
    shell.update(shell.entries, 3, { unlisted: true });
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(within(panel()!).getByTestId("agent-card-gone")).toBeInTheDocument();
    // Its cards listed, without this one: the pane goes.
    shell.update(shell.entries.filter((entry) => entry.key !== ended.key), 4);
    expect(screen.queryByTestId("agent-pane-overlay")).toBeNull();
    expect(screen.queryByTestId("ended-card-pane")).toBeNull();
  });

  /// Scenario: A card the deck keeps for an agent that is gone still lists an enabled action and a question with an answer button, as a card can for a moment while the deck catches up. Open it: the read-only pane's Card tab shows the card's report and question but no action icon and no answer button, says its actions need a running agent, and nothing is ever sent.
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

  /// Scenario: Open a working agent's pane from its card and switch to its Card tab. The agent goes while the deck keeps its stopped card, so the pane becomes the read-only ended pane. The daemon stops answering: the pane stays the ended pane, not the agent's earlier live pane. The daemon answers again, still without the agent: still the ended pane. When the deck lists the agent again, its live pane with its terminal is back.
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
    expect(shownTab()).toBe(cardTab());
    expect(panel()?.getAttribute("data-card-key")).toBe(stopped.key);
    // A fresh record of the same agent: its live pane again.
    shell.update(shell.entries, 5);
    expect(screen.queryByTestId("ended-card-pane")).toBeNull();
    expect(within(pane()).getByTestId(`terminal-${first.id}`)).toBeInTheDocument();
  });

  /// Scenario: Open the first agent from its card and switch to its Card tab, then let its daemon stop answering, and answer again. While it does not answer, the pane keeps its Card tab, which says the card is not available right now and offers none of its actions, and the terminal is never resized; when the daemon answers again, the Card tab shows the card again.
  it("desktop/agent-cards/105: a disconnect keeps the shown Card tab and says the card is not available", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    fireEvent.click(card(shell.entries[0]));
    fireEvent.click(cardTab());
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    const resized = shell.resizeTerminal.mock.calls.length;
    shell.update(shell.entries, 2, { connected: false });
    expect(screen.getByTestId("agent-pane-overlay")).toBeInTheDocument();
    expect(shownTab()).toBe(cardTab());
    expect(within(panel()!).getByTestId("agent-card-gone")).toHaveTextContent("This card is not available right now.");
    expect(within(panel()!).queryByTestId("agent-card-panel-actions")).toBeNull();
    expect(within(panel()!).queryByRole("button")).toBeNull();
    shell.update(shell.entries, 3);
    expect(panel()?.getAttribute("data-card-key")).toBe(shell.entries[0].key);
    expect(within(panel()!).getByText("first-card is working")).toBeInTheDocument();
    expect(shell.resizeTerminal.mock.calls.length).toBe(resized);
  });

  /// Scenario: In the Cards view, the first agent's card has more checked pull requests and issues than its reference line holds and more actions than its foot shows. Press the references' "+N": the agent's pane opens on its Card tab, scrolled to its pull requests and issues with the keyboard on them. Back on the dashboard, press the actions' "+N": the pane opens on its Card tab with its Actions in view. Opened from the card's body afterwards, the pane is on Terminal again.
  it("desktop/agent-cards/128: a card's +N opens the pane on its Card tab, at what it counted", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    // jsdom lays nothing out: give the reference line 200 px and each badge 60 px with a 6 px gap.
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.classList.contains("agent-card-refs") ? 200 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockImplementation(function width(this: HTMLElement) { return this.classList.contains("agent-card-ref") ? 60 : 0; });
    vi.spyOn(HTMLElement.prototype, "offsetLeft", "get").mockImplementation(function left(this: HTMLElement) { return this.classList.contains("agent-card-ref") ? [...(this.parentElement?.children ?? [])].indexOf(this) * 66 : 0; });
    const scrolled = vi.fn();
    Element.prototype.scrollIntoView = scrolled;
    try {
      const shell = renderShell({ deck: { tree: true } });
      const [first] = shell.agents;
      const ref = (number: number) => ({ field: `pull-request-${number}`, kind: "pull_request", claim: `#${number}`, repo: "o/r", number, url: `https://github.com/o/r/pull/${number}`, result: { status: "checked", state: "open", checked_at: "2026-11-26T00:00:00Z", stale: false } });
      const many = cardFor(first.id, "first-card", {
        revision: 2,
        checked_refs: [1, 2, 3, 4, 5].map(ref),
        actions: ["a1", "a2", "a3", "a4", "a5"].map((id) => action(id, { label: `Action ${id}`, icon: "play" })),
      } as Partial<AgentCardEntry>);
      shell.update([many, shell.entries[1], shell.entries[2]], 2, { tree: true });

      fireEvent.click(within(card(many)).getByTestId("agent-card-refs-more"));
      expect(shownTab()).toBe(cardTab());
      const refs = within(panel()!).getByTestId("agent-card-panel-refs");
      expect(scrolled.mock.contexts).toContain(refs);
      expect(refs).toHaveFocus();
      expect(within(refs).getAllByTestId("agent-card-panel-ref")).toHaveLength(5);
      back();

      scrolled.mockClear();
      fireEvent.click(within(card(many)).getByTestId("agent-card-actions-more"));
      expect(shownTab()).toBe(cardTab());
      const actions = within(panel()!).getByTestId("agent-card-panel-actions");
      expect(scrolled.mock.contexts).toContain(actions);
      expect(actions).toHaveFocus();
      back();

      fireEvent.click(card(many));
      expectOnTerminal(first.id, "the same card's body afterwards");
    } finally {
      delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView;
      vi.restoreAllMocks();
    }
  });

  /// Scenario: An agent reports a `url`, a `pull_request` and an `issue` field, each an `https://` address it chose, and the user opens its pane from its card and switches to the Card tab. The Card tab shows each address as plain text the user can select and copy, not as a link.
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
    fireEvent.click(cardTab());
    const fields = panel()!.querySelector<HTMLElement>("dl[aria-label='Fields']")!;
    for (const value of ["https://agent-controlled.invalid/", "https://github.com/o/r/pull/7", "https://github.com/o/r/issues/3"]) {
      expect(within(fields).getByText(value).closest("a")).toBeNull();
    }
    expect(fields.querySelector("a")).toBeNull();
    expect(panel()!.querySelector("a[href='https://agent-controlled.invalid/']")).toBeNull();
  });

  /// Scenario: Open an agent's pane from its card on a daemon whose card actions can be edited, and switch to its Card tab. Beside the Actions heading is a small Edit button, named "Edit card actions"; pressing it opens that daemon's Card actions editor over the pane and reads its actions. The read-only pane of an agent that has ended shows no Edit, and neither does the Card tab of an agent whose daemon offers no Card actions editor (PR #2491, 2026-11-25).
  it("desktop/agent-cards/118: the Card tab's Edit opens its daemon's Card actions editor, and the ended pane has none", async () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const goneId = "gone-118";
    const ended = cardFor(goneId, "ended-118", { status: "stopped", actions: [action("go-ahead", { label: "Go ahead", enabled: true })] });
    const shell = renderShell({ extra: [ended], deck: { catalogEdit: true } });
    fireEvent.click(card(shell.entries[0]));
    fireEvent.click(cardTab());
    const actions = within(panel()!).getByTestId("agent-card-panel-actions");
    const heading = within(actions).getByRole("heading", { name: /^Actions/ });
    const edit = within(actions).getByRole("button", { name: "Edit card actions" });
    expect(edit).toHaveTextContent("Edit");
    expect(edit).toHaveAttribute("title", "Edit card actions");
    expect(edit).toHaveAttribute("type", "button");
    expect(heading.contains(edit) || heading.parentElement === edit.parentElement, "beside the Actions heading").toBe(true);
    await act(async () => { fireEvent.click(edit); });
    const editor = screen.getByRole("dialog", { name: /^Card actions: / });
    expect(editor).toBeInTheDocument();
    expect(shell.readCardCatalog).toHaveBeenCalledExactlyOnceWith(FIXTURE_DAEMON_ID, INSTANCE);
    await act(async () => { fireEvent.click(within(editor).getByTestId("agent-card-actions-editor-close")); });
    back();

    fireEvent.click(card(ended));
    expect(screen.getByTestId("ended-card-pane")).toBeInTheDocument();
    expect(within(panel()!).queryByRole("button", { name: "Edit card actions" })).toBeNull();
  });

  /// Scenario: Open an agent's pane from its card on a daemon from a release whose card actions cannot be edited, and switch to its Card tab. It still has Edit beside Actions; pressing it opens that daemon's Card actions editor read-only, saying why, as its ⋯ menu does, and reads nothing.
  it("desktop/agent-cards/118: the Card tab's Edit opens a daemon that cannot edit card actions read-only", async () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    fireEvent.click(card(shell.entries[0]));
    fireEvent.click(cardTab());
    await act(async () => { fireEvent.click(within(panel()!).getByRole("button", { name: "Edit card actions" })); });
    const editor = screen.getByRole("dialog", { name: /^Card actions: / });
    expect(within(editor).getByTestId("agent-card-actions-editor-readonly")).toHaveTextContent(/earlier release, so its card actions cannot be edited from the app/);
    expect(shell.readCardCatalog).not.toHaveBeenCalled();
  });

  /// Scenario: Open the first agent from its card and use the header: Next agent opens the second agent's pane, Previous agent the first's, Next needing you the third's, which needs you. With the keyboard outside the terminal, `]`, `[` and `n` do the same; with the keyboard in the terminal, they are the user's typing and move nothing.
  it("desktop/agent-cards/096: the pane header moves between agents in the dashboard's order, and its keys stay out of the terminal", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const shell = renderShell();
    const [first, second, third] = shell.agents;
    fireEvent.click(card(shell.entries[0]));
    const shownAgent = () => pane().querySelector("textarea[data-testid^='terminal-']")?.getAttribute("data-testid");
    expect(shownAgent()).toBe(`terminal-${first.id}`);
    next();
    expect(shownAgent()).toBe(`terminal-${second.id}`);
    previous();
    expect(shownAgent()).toBe(`terminal-${first.id}`);
    nextNeedingYou();
    expect(shownAgent()).toBe(`terminal-${third.id}`);

    // The keys, with the keyboard on the pane but not in its terminal.
    act(() => { within(pane()).getByRole("button", { name: "Next agent" }).focus(); });
    fireEvent.keyDown(document.activeElement!, { key: "[" });
    expect(shownAgent()).toBe(`terminal-${second.id}`);
    fireEvent.keyDown(document.activeElement!, { key: "]" });
    expect(shownAgent()).toBe(`terminal-${third.id}`);
    fireEvent.keyDown(document.activeElement!, { key: "n" });
    expect(shownAgent()).toBe(`terminal-${third.id}`);

    // In the terminal, the same keys move nothing: they are the agent's.
    const terminal = within(pane()).getByTestId(`terminal-${third.id}`);
    act(() => { terminal.focus(); });
    for (const key of ["[", "]", "n"]) fireEvent.keyDown(terminal, { key });
    expect(within(pane()).getByTestId(`terminal-${third.id}`)).toBe(terminal);
  });

  /// Scenario: Open an agent's pane: its header has Previous agent, Next agent and Next needing you, and no Card button. Nothing the app stores remembers a Card choice, and opening the Card tab writes nothing to storage.
  it("desktop/agent-cards/125: the pane header has no Card button and the Card tab stores no choice", () => {
    const shell = renderShell();
    openRow(shell.agents[0].id);
    expect(within(pane()).queryByTestId("agent-pane-card-toggle")).toBeNull();
    expect(within(pane()).queryByRole("button", { name: /^Card$|card/i })).toBeNull();
    for (const name of ["Previous agent", "Next agent"]) expect(within(pane()).getByRole("button", { name })).toBeInTheDocument();
    expect(within(pane()).getByRole("button", { name: /Next needing you/ })).toBeInTheDocument();
    const before = { ...window.localStorage };
    fireEvent.click(cardTab());
    fireEvent.click(terminalTab());
    expect({ ...window.localStorage }).toEqual(before);
    expect(window.localStorage.getItem("dot-agent-deck.agent-pane.card-panel")).toBeNull();
  });
});
