import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { OVERVIEW_MODE_STORAGE_KEY } from "../lib/agentCards";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";

/*
 * PRD #2374 — the dashboard's cards as the overview wires them: the switch,
 * the grid in the daemon's order, the primary action, the overlay and the
 * table row's additions. The spec tests of the contract (`desktop/agent-cards/00x`)
 * are the tester's; these are the coder's working checks.
 */

const INSTANCE = "daemon-instance-1";

function action(id: string, overrides: Partial<AgentCardAction> = {}): AgentCardAction {
  return {
    id,
    label: id,
    kind: "prompt",
    prompt: `Do ${id}.`,
    text_input: false,
    confirm: false,
    confirm_detail: null,
    icon: null,
    placement: "card",
    custom: false,
    source: "catalog",
    enabled: true,
    disabled_reason: null,
    last_delivery: null,
    token: `tok-${id}`,
    ...overrides,
  };
}

const DECK_ACTIONS = [
  action("reply", { label: "Reply", prompt: null, text_input: true, source: "deck", placement: "detail" }),
  action("open", { label: "Open", kind: "open", prompt: null, source: "deck", placement: "detail" }),
  action("close", { label: "Close", kind: "close", prompt: null, confirm: true, confirm_detail: "Still pending: an unanswered question.", source: "deck", placement: "detail" }),
];

function entry(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`,
    instance_id: INSTANCE,
    pane_id: `pane-${agentId}`,
    agent_id: agentId,
    agent_type: "claude_code",
    name,
    status: "working",
    needs_you: false,
    needs_you_reasons: [],
    group: { id: "standalone", kind: "standalone", label: null },
    relations: {},
    report: null,
    fields: [],
    questions: [],
    actions: [...DECK_ACTIONS],
    primary_action_id: "open",
    facts: { source: "deck", pull_request: null },
    revision: 1,
    updated_at: "2026-11-17T00:00:00Z",
    ...overrides,
  };
}

const CODER = entry("7", "coder", {
  status: "waiting_for_input",
  needs_you: true,
  needs_you_reasons: ["question", "waiting"],
  group: { id: "dispatcher:p1/1", kind: "dispatcher", label: "dispatcher" },
  report: { source: "agent", status: "waiting", summary: "Which base branch?", published_at: "2026-11-17T00:00:00Z", revision: 2 },
  fields: [{ name: "progress", label: "Progress", description: "What you are doing now.", type: "text", icon: null, placement: "card", custom: false, source: "agent", value: "tests pass" }],
  questions: [{ id: "base", text: "Which base branch?", source: "agent", options: [{ id: "main", label: "main", action_id: "question/base/main" }], answered: null }],
  actions: [action("question/base/main", { label: "main", prompt: "Answer to your question \"Which base branch?\": main", source: "deck" }), action("merge", { label: "Merge it", prompt: "Merge it, then update your card.", confirm: true }), ...DECK_ACTIONS],
  primary_action_id: "question/base/main",
});
const TESTER = entry("8", "tester", { group: { id: "dispatcher:p1/1", kind: "dispatcher", label: "dispatcher" } });
const LONE = entry("9", "lone");

function fleet(): DeckFleet {
  const deck = createFixtureFleet("fleet")[0];
  const agents = [
    createFixtureStartedAgent({ id: "7", daemonId: FIXTURE_DAEMON_ID, displayName: "coder", command: "claude", cwd: "/w" }),
    createFixtureStartedAgent({ id: "8", daemonId: FIXTURE_DAEMON_ID, displayName: "tester", command: "claude", cwd: "/w" }),
    createFixtureStartedAgent({ id: "9", daemonId: FIXTURE_DAEMON_ID, displayName: "lone", command: "claude", cwd: "/w" }),
  ];
  return [{ ...deck, agents }];
}

function snapshot(entries: AgentCardEntry[]): AgentCardsSnapshot {
  return { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision: 3, entries, catalogErrors: [] };
}

function runtime(overrides: Partial<DeckRuntimeState> = {}): DeckRuntimeState {
  const decks = fleet();
  return {
    mode: "fixture",
    snapshot: decks[0],
    fleet: decks,
    terminalData: {},
    clearError: vi.fn(),
    runAction: vi.fn(),
    sendTerminalInput: vi.fn(async () => undefined),
    resizeTerminal: vi.fn(async () => undefined),
    setShownTerminals: vi.fn(async () => undefined),
    reconnect: vi.fn(async () => undefined),
    listProjects: vi.fn(async () => ({ projects: [] })),
    resolveProject: vi.fn(async () => { throw new Error("unresolved"); }),
    getSettings: vi.fn(),
    saveSettings: vi.fn(),
    testEndpoint: vi.fn(),
    secretStatus: vi.fn(),
    storeSecret: vi.fn(),
    forgetSecret: vi.fn(),
    setZoom: vi.fn(async (level: number) => level),
    desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: { [FIXTURE_DAEMON_ID]: snapshot([CODER, TESTER, LONE]) },
    invokeCardAction: vi.fn(async (_target, _actionId, options) => ({ request_id: options?.requestId ?? "r", outcome: "sent", message_id: "m-1" })),
    ...overrides,
  } as DeckRuntimeState;
}

beforeEach(() => window.localStorage.clear());
afterEach(() => {
  window.localStorage.clear();
  act(() => setDashboardFilter(clearDashboardFilter()));
});

describe("the dashboard's cards (PRD #2374)", () => {
  it("switches to cards, remembers it, and shows the daemon's groups in its order", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    expect(window.localStorage.getItem(OVERVIEW_MODE_STORAGE_KEY)).toBe("cards");
    const groups = screen.getAllByTestId("agent-cards-group");
    expect(groups).toHaveLength(2);
    expect(within(groups[0]).getByTestId("agent-cards-group-count").textContent).toBe("2 agents · 1 needs you");
    expect(within(groups[0]).getAllByTestId("agent-card-show").map((button) => button.textContent)).toEqual(["coder", "tester"]);
    fireEvent.click(screen.getByTestId("agent-cards-needs-you-filter"));
    expect(screen.getAllByTestId("agent-card-show").map((button) => button.textContent)).toEqual(["coder"]);
  });

  /*
    PR #2378 review (Qodo): the Cards view keeps each daemon's own section
    and header — its name and the daemon's controls (Stop daemon, Upgrade,
    host metrics, New agent on this daemon) — with the cards as its body.
  */
  it("keeps each daemon's header and its controls above that daemon's cards", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const headerControls = () => [...document.querySelectorAll(".daemon-group-header button")].map((button) => button.getAttribute("data-testid") ?? button.textContent);
    const inTable = headerControls();
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    expect(headerControls()).toEqual(inTable);
    const identity = screen.getByTestId("daemon-identity");
    const section = identity.closest(".daemon-group");
    expect(section).not.toBeNull();
    for (const group of screen.getAllByTestId("agent-cards-group")) expect(section?.contains(group)).toBe(true);
  });

  it("falls back to the table when storage throws", () => {
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("blocked"); });
    try {
      render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
      expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
      expect(screen.queryByTestId("agent-cards")).toBeNull();
    } finally {
      getItem.mockRestore();
    }
  });

  it("sends the primary action as the daemon named it and shows its prompt", async () => {
    const live = runtime();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    const coder = screen.getAllByTestId("agent-card-primary")[0];
    expect(coder.textContent).toBe("main");
    await act(async () => { fireEvent.click(coder); });
    expect(live.invokeCardAction).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: "pane-7", agentId: "7" }, "question/base/main", expect.objectContaining({ requestId: expect.any(String) }));
    expect(screen.getByTestId("agent-card-result").textContent).toContain("Sent");
  });

  it("asks before a confirm action, then sends it confirmed", async () => {
    const live = runtime();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getAllByTestId("agent-card-show")[0]);
    fireEvent.click(screen.getByTestId("agent-card-action-merge"));
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-card-confirm").textContent).toContain("Merge it, then update your card.");
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: "7" }), "merge", expect.objectContaining({ confirm: true }));
  });

  it("moves through cards in display order and says when the open one is gone", () => {
    const live = runtime();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    const view = render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getAllByTestId("agent-card-show")[0]);
    const overlay = () => screen.getByTestId("agent-card-overlay");
    expect(overlay().getAttribute("data-card-key")).toBe("pane-7/7");
    fireEvent.keyDown(overlay(), { key: "]" });
    expect(overlay().getAttribute("data-card-key")).toBe("pane-8/8");
    fireEvent.keyDown(overlay(), { key: "[" });
    expect(overlay().getAttribute("data-card-key")).toBe("pane-7/7");
    fireEvent.keyDown(overlay(), { key: "]" });
    fireEvent.keyDown(overlay(), { key: "n" });
    expect(overlay().getAttribute("data-card-key")).toBe("pane-7/7");
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([TESTER, LONE]) } }} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("agent-card-gone").textContent).toBe("This agent was closed.");
    fireEvent.keyDown(overlay(), { key: "Escape" });
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
  });

  it("marks a table row that needs you and opens its card from the row menu, while a row click still opens the agent", () => {
    const navigate = vi.fn();
    render(<AgentOverview runtime={runtime()} onNavigate={navigate} />);
    const coderRow = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "7" })}`);
    expect(within(coderRow).getByTestId("overview-needs-you")).toBeTruthy();
    fireEvent.click(within(coderRow).getByTestId("overview-row-menu"));
    expect(navigate).not.toHaveBeenCalled();
    fireEvent.click(within(coderRow).getByTestId("overview-row-show-card"));
    expect(screen.getByTestId("agent-card-overlay").getAttribute("data-card-key")).toBe("pane-7/7");
    fireEvent.keyDown(screen.getByTestId("agent-card-overlay"), { key: "Escape" });
    fireEvent.click(coderRow.querySelector("td:last-child") ?? coderRow);
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: "7" }));
  });

  /* PRD #2374 review (Greptile P1): Open closes the card, so the pane it
     opens is not hidden behind the overlay. */
  it("closes the card when its Open action opens the agent", () => {
    const navigate = vi.fn();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime()} onNavigate={navigate} />);
    fireEvent.click(screen.getAllByTestId("agent-card-show")[0]);
    fireEvent.click(screen.getByTestId("agent-card-action-open"));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: "7" }));
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
  });

  /* PRD #2374 review (Greptile P2): the dashboard filter shows the cards of
     the agents it matches, in the daemon's order, and nothing else. */
  it("applies the dashboard filter to the cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const shown = () => screen.getAllByTestId("agent-card-show").map((button) => button.textContent);
    expect(shown()).toEqual(["coder", "tester", "lone"]);
    act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "tester" }));
    expect(shown()).toEqual(["tester"]);
    act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "o" }));
    expect(shown()).toEqual(["coder", "lone"]);
    act(() => setDashboardFilter(clearDashboardFilter()));
    expect(shown()).toEqual(["coder", "tester", "lone"]);
  });

  /* PRD #2374 review (Qodo): the compact card shows only the start of a
     prompt. An agent-written one it cannot show whole opens the review step;
     one it shows whole, or the catalog's, is sent by the button. */
  it("reviews an agent-written primary the card cannot show whole, and sends one it can", async () => {
    const long = "Use the release branch. Then also delete every stale branch on the remote, and force-push.";
    const agentWritten = (prompt: string, promptSource: string) => entry("7", "coder", {
      needs_you: true,
      needs_you_reasons: ["question"],
      actions: [action("question/base/rel", { label: "release", prompt, source: "agent", prompt_source: promptSource }), ...DECK_ACTIONS],
      primary_action_id: "question/base/rel",
    });
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");

    const reviewed = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([agentWritten(long, "agent")]) } });
    const view = render(<AgentOverview runtime={reviewed} onNavigate={vi.fn()} />);
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-primary")); });
    expect(reviewed.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-card-confirm").textContent).toContain(long);
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(reviewed.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: "7" }), "question/base/rel", expect.objectContaining({ actionToken: "tok-question/base/rel" }));
    view.unmount();

    for (const [prompt, source] of [["Use the release branch.", "agent"], [long, "catalog"]]) {
      const sent = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([agentWritten(prompt, source)]) } });
      const shown = render(<AgentOverview runtime={sent} onNavigate={vi.fn()} />);
      await act(async () => { fireEvent.click(screen.getByTestId("agent-card-primary")); });
      expect(sent.invokeCardAction).toHaveBeenCalledTimes(1);
      expect(screen.queryByTestId("agent-card-confirm")).toBeNull();
      shown.unmount();
    }
  });

  it("shows none of it when the runtime offers no cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime({ agentCards: undefined })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("overview-mode-switch")).toBeNull();
    expect(screen.queryByTestId("agent-cards")).toBeNull();
    expect(screen.queryByTestId("overview-needs-you")).toBeNull();
    expect(screen.queryByTestId("overview-row-menu")).toBeNull();
  });
});

describe("a dashboard row's Needs you and controls (issue #2454)", () => {
  const row = (id: string) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id })}`);
  const withPullRequest = (): DeckRuntimeState => {
    const live = runtime();
    const decks = live.fleet.map((deck) => ({ ...deck, agents: deck.agents.map((agent) => agent.id === "7" ? { ...agent, pullRequest: { number: 2454, url: "https://github.com/o/r/pull/2454", state: "open" as const, review: "review_required" as const } } : agent) }));
    return { ...live, snapshot: decks[0], fleet: decks };
  };

  /// Scenario: Show the dashboard table with cards on, where coder needs you and has a pull request and tester does not need you. Coder's Needs you is an amber badge in the name cell right after its PR badge, with the daemon's reasons as its hover text and no button role; tester has none; neither row's control group holds it, and no row has an Open or Generate config icon.
  it("draws Needs you as a name-cell badge after the PR badge, not among the row's controls", () => {
    render(<AgentOverview runtime={withPullRequest()} onNavigate={vi.fn()} />);
    const coder = row("7");
    const badge = within(coder).getByTestId("overview-needs-you");
    expect(badge.textContent).toBe("Needs you");
    expect(badge.getAttribute("title")).toBe("Needs you: the agent asked a question on its card; the agent says it is waiting for you.");
    expect(badge.tagName).toBe("SPAN");
    expect(badge).not.toHaveAttribute("role");
    expect(badge).not.toHaveAttribute("tabindex");
    const nameCell = coder.querySelector(".overview-agent-name")!;
    expect(badge.parentElement).toBe(nameCell);
    expect(badge.previousElementSibling).toBe(within(coder).getByTestId("overview-pr-badge"));
    const actions = nameCell.querySelector(".overview-row-actions")!;
    expect(actions.contains(badge)).toBe(false);
    expect([...actions.children].map((child) => child.className)).toEqual(["overview-stop-agent", "overview-card-menu"]);

    const tester = row("8");
    expect(within(tester).queryByTestId("overview-needs-you")).toBeNull();
    expect([...tester.querySelector(".overview-row-actions")!.children].map((child) => child.className)).toEqual(["overview-stop-agent", "overview-card-menu"]);
    for (const each of [coder, tester, row("9")]) {
      expect(within(each).queryByRole("button", { name: /^Open /i })).toBeNull();
      expect(within(each).queryByRole("button", { name: /config with/i })).toBeNull();
    }
  });

  /// Scenario: Click coder's Needs you badge, then focus coder's row and press Enter. Both open coder's pane, since the badge is a label inside the row and the row is the keyboard route named "Open coder agent".
  it("opens the agent from a click on the badge and from Enter on the row", () => {
    const navigate = vi.fn();
    render(<AgentOverview runtime={runtime()} onNavigate={navigate} />);
    fireEvent.click(within(row("7")).getByTestId("overview-needs-you"));
    expect(navigate).toHaveBeenCalledOnce();
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "agent", agentId: "7" }));
    navigate.mockClear();
    const coder = screen.getByRole("row", { name: "Open coder agent" });
    expect(coder).toBe(row("7"));
    coder.focus();
    fireEvent.keyDown(coder, { key: "Enter" });
    expect(navigate).toHaveBeenCalledOnce();
    navigate.mockClear();
    fireEvent.keyDown(within(coder).getByTestId("overview-row-menu"), { key: "Enter" });
    expect(navigate).not.toHaveBeenCalled();
  });

  /// Scenario: The daemon's next card list clears coder's Needs you (the user dealt with it). The badge leaves coder's name cell and the row's controls stay exactly as they were.
  it("removes the badge when the daemon clears Needs you, leaving the controls as they were", () => {
    const live = runtime();
    const view = render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
    const before = [...row("7").querySelector(".overview-row-actions")!.children].map((child) => child.className);
    expect(within(row("7")).getByTestId("overview-needs-you")).toBeTruthy();
    const cleared = { ...CODER, needs_you: false, needs_you_reasons: [] };
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([cleared, TESTER, LONE]) } }} onNavigate={vi.fn()} />);
    expect(within(row("7")).queryByTestId("overview-needs-you")).toBeNull();
    expect([...row("7").querySelector(".overview-row-actions")!.children].map((child) => child.className)).toEqual(before);
  });
});
