import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { createElement } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { approvedProjectTrust } from "./test/projectTrust";
import { createFixtureSnapshot, FIXTURE_DAEMON_ID } from "./data/fixture";
import { DEFAULT_DESKTOP_SETTINGS, fixtureDesktopFeatures, type DesktopSettingsDto } from "./lib/bridge";
import { CARD_PANEL_STORAGE_KEY } from "./lib/paneCards";
import type { AgentCardAction, AgentCardEntry, DeckActionResult, DeckRuntimeState } from "./types";

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

/** The real shell on the dashboard, with a card for each of the fixture deck's first three agents; the third needs you. */
function renderShell() {
  const base = createFixtureSnapshot("connected");
  const snapshot = { ...base, connection: { ...base.connection, incarnation: INSTANCE }, agents: base.agents.map((agent) => ({ ...agent, incarnation: INSTANCE })) };
  const [first, second, third] = snapshot.agents;
  const entries = [cardFor(first.id, "first-card"), cardFor(second.id, "second-card"), cardFor(third.id, "third-card", { needs_you: true, needs_you_reasons: ["question"] })];
  const settings = settingsStore();
  const runtime = (cards: AgentCardEntry[], revision: number) => ({
    mode: "fixture",
    desktopFeatures: fixtureDesktopFeatures("?experimental=1"),
    snapshot,
    fleet: [snapshot],
    terminalData: {},
    clearError: vi.fn(),
    ...approvedProjectTrust(),
    runAction: vi.fn(async () => ({ ok: true }) as DeckActionResult),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved: no such project"); }),
    setZoom: vi.fn(async (level: number) => level),
    getSettings: settings.getSettings,
    saveSettings: settings.saveSettings,
    agentCards: { [FIXTURE_DAEMON_ID]: { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision, entries: cards, catalogErrors: [] } },
    invokeCardAction: vi.fn(async (_target: unknown, _actionId: string, options?: { requestId?: string }) => ({ request_id: options?.requestId ?? "r", outcome: "sent" as const, message_id: "m-pane-1" })),
  }) as unknown as DeckRuntimeState;
  const view = render(createElement(DeckShell, { runtime: runtime(entries, 1), initialView: { kind: "overview" } }));
  return {
    agents: [first, second, third],
    entries,
    update: (cards: AgentCardEntry[], revision: number) => view.rerender(createElement(DeckShell, { runtime: runtime(cards, revision), initialView: { kind: "overview" } })),
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

  /// Scenario: Open the first agent from its card and use the header: Next agent opens the second agent's pane, Previous agent the first's, Next needing you the third's, which needs you, each with its Card panel as the user had it. With the keyboard outside the terminal, `]`, `[` and `n` do the same; with the keyboard in the terminal, they are the user's typing and move nothing.
  it("desktop/agent-cards/096: the pane header moves between agents in the dashboard's order, and its keys stay out of the terminal", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
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
});
