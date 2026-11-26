import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { OVERVIEW_MODE_STORAGE_KEY } from "../lib/agentCards";
import { clearDashboardFilter, setDashboardFilter } from "../lib/dashboardFilter";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";
import { OverviewWithCardPane } from "../test/cardPane";
import { cardsViewShown, pressNeedsYouOnly } from "../test/needsYouMenu";

/*
 * PRD #2374 — the dashboard's cards as the overview wires them: the switch,
 * the grid in the daemon's order, the overlay actions and the
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
const shownNames = () => [...document.querySelectorAll("article[data-card-key]")].map((card) => card.getAttribute("aria-label")?.replace(/, needs you$/, ""));

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
  /// Scenario: switch to Cards, then toggle Needs you only from the top bar's ⋯ menu. The preference is remembered, the ⋯ counts the one agent that needs the user, and the daemon's card order stays put while unmarked cards fade.
  it("switches to cards, remembers it, and shows the daemon's cards in its order", () => {
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    expect(window.localStorage.getItem(OVERVIEW_MODE_STORAGE_KEY)).toBe("cards");
    expect(shownNames()).toEqual(["coder", "tester", "lone"]);
    expect(screen.getByTestId("overview-needs-you-count").textContent).toBe("1");
    // Needs you only fades the cards the daemon did not mark, in place (issue #2418).
    pressNeedsYouOnly();
    expect(shownNames()).toEqual(["coder", "tester", "lone"]);
    expect([...document.querySelectorAll("article[data-faded='true']")].map((node) => node.getAttribute("aria-label"))).toEqual(["tester", "lone"]);
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
    expect(section?.contains(screen.getByTestId("agent-cards-tree"))).toBe(true);
  });

  it("falls back to the table when storage throws", () => {
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("blocked"); });
    try {
      render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
      expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
      expect(cardsViewShown()).toBe(false);
    } finally {
      getItem.mockRestore();
    }
  });

  /// Scenario: press a question's answer button in the agent's Card tab with a catalog or non-catalog prompt. Only the catalog answer sends directly; the others show the exact answer in the single-action review and wait for Send before delivering to the correct agent, and the panel says it was sent.
  it.each(["agent", "deck", "catalog", undefined])("reviews the question's non-catalog answer before sending it from the Card tab (source %s)", async (promptSource) => {
    const card = { ...CODER, actions: CODER.actions.map((action) => action.id === "question/base/main" ? { ...action, prompt_source: promptSource } : action) };
    const live = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([card]) } });
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<OverviewWithCardPane runtime={live} />);
    fireEvent.click(screen.getAllByTestId("agent-card-details")[0]);
    const panel = screen.getByTestId("agent-card-panel");
    const coder = within(panel).getByRole("button", { name: "main" });
    expect(coder.textContent).toBe("main");
    await act(async () => { fireEvent.click(coder); });
    if (promptSource === "catalog") {
      expect(screen.queryByTestId("agent-card-confirm")).toBeNull();
    } else {
      expect(live.invokeCardAction).not.toHaveBeenCalled();
      expect(screen.getByTestId("agent-card-confirm-prompt").textContent).toBe("Answer to your question \"Which base branch?\": main");
      await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    }
    expect(live.invokeCardAction).toHaveBeenCalledTimes(1);
    expect(live.invokeCardAction).toHaveBeenCalledWith({ deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: "pane-7", agentId: "7" }, "question/base/main", expect.objectContaining({ requestId: expect.any(String) }));
    expect(within(screen.getByTestId("agent-card-panel")).getByTestId("agent-card-action-result").textContent).toContain("Sent");
  });

  /// Scenario: press, in the agent's Card tab, the icon of an action that asks first. Nothing is sent until the single-action review, showing the exact text, is answered with Send, which sends it confirmed.
  it("asks before a confirm action, then sends it confirmed", async () => {
    const live = runtime();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<OverviewWithCardPane runtime={live} />);
    fireEvent.click(screen.getAllByTestId("agent-card-details")[0]);
    fireEvent.click(within(screen.getByTestId("agent-card-panel")).getByRole("button", { name: "Merge it" }));
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-card-confirm").textContent).toContain("Merge it, then update your card.");
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: "7" }), "merge", expect.objectContaining({ confirm: true }));
  });

  /// Scenario: open the coder's Card tab and move with the pane's next, previous and Next needing you. The pane follows the display order, Next needing you comes back to the coder, and once the coder's card leaves the list the panel says the card is not available right now.
  it("moves through agents in display order and says when the shown one has no card", () => {
    const live = runtime();
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    const view = render(<OverviewWithCardPane runtime={live} />);
    fireEvent.click(screen.getAllByTestId("agent-card-details")[0]);
    const panel = () => screen.getByTestId("agent-card-panel");
    expect(panel().getAttribute("data-card-key")).toBe("pane-7/7");
    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    expect(panel().getAttribute("data-card-key")).toBe("pane-8/8");
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    expect(panel().getAttribute("data-card-key")).toBe("pane-7/7");
    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    fireEvent.click(screen.getByRole("button", { name: "Next agent that needs you" }));
    expect(panel().getAttribute("data-card-key")).toBe("pane-7/7");
    view.rerender(<OverviewWithCardPane runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([TESTER, LONE]) } }} />);
    expect(screen.getByTestId("agent-card-gone").textContent).toContain("This card is not available right now.");
  });

  /// Scenario: in the Table view, read the coder's row, which needs you, open its ⋯ menu and press Show card, then click another row. The menu opens nothing by itself; Show card opens the coder's pane, on Terminal as every way into an agent does; a row click opens that agent's pane the same way; neither asks for the Card tab.
  it("marks a table row that needs you and opens its card from the row menu, while a row click still opens the agent", () => {
    const navigate = vi.fn();
    render(<OverviewWithCardPane runtime={runtime()} onNavigate={navigate} />);
    const coderRow = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "7" })}`);
    expect(within(coderRow).getByTestId("overview-needs-you")).toBeTruthy();
    fireEvent.click(within(coderRow).getByTestId("overview-row-menu"));
    expect(navigate).not.toHaveBeenCalled();
    fireEvent.click(within(coderRow).getByTestId("overview-row-show-card"));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "agent", agentId: "7" }));
    expect(navigate.mock.lastCall?.[0]).not.toHaveProperty("cardTab");
    expect(screen.getByTestId("agent-card-panel").getAttribute("data-card-key")).toBe("pane-7/7");
    const testerRow = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: "8" })}`);
    fireEvent.click(testerRow.querySelector("td:last-child") ?? testerRow);
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "agent", agentId: "8" }));
    expect(navigate.mock.lastCall?.[0]).not.toHaveProperty("cardTab");
  });

  /* PRD #2374 review (Greptile P2): the dashboard filter shows the cards of
     the agents it matches, in the daemon's order, and nothing else. */
  /// Scenario: filter the Cards view by tester, then by a shared name fragment, then clear it. The visible card names follow the filter while retaining the daemon's order.
  it("applies the dashboard filter to the cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    const shown = shownNames;
    expect(shown()).toEqual(["coder", "tester", "lone"]);
    act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "tester" }));
    expect(shown()).toEqual(["tester"]);
    act(() => setDashboardFilter({ ...clearDashboardFilter(), text: "o" }));
    expect(shown()).toEqual(["coder", "lone"]);
    act(() => setDashboardFilter(clearDashboardFilter()));
    expect(shown()).toEqual(["coder", "tester", "lone"]);
  });

  /// Scenario: press, in the agent's Card tab, the icon of an agent-written and of a catalog action, with long and short prompts. Every non-catalog prompt requires the single-action review's Send, while catalog prompts send directly.
  it("reviews complete non-catalog prompts and sends only catalog prompts directly", async () => {
    const long = "Use the release branch. Then also delete every stale branch on the remote, and force-push.";
    const agentWritten = (prompt: string, promptSource: string) => entry("7", "coder", {
      needs_you: true,
      needs_you_reasons: ["question"],
      actions: [action("question/base/rel", { label: "release", prompt, source: "agent", prompt_source: promptSource }), ...DECK_ACTIONS],
      primary_action_id: "question/base/rel",
    });
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    const releaseIcon = () => within(screen.getByTestId("agent-card-panel")).getByRole("button", { name: "release" });

    const reviewed = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([agentWritten(long, "agent")]) } });
    const view = render(<OverviewWithCardPane runtime={reviewed} />);
    fireEvent.click(screen.getByTestId("agent-card-details"));
    expect(releaseIcon().getAttribute("data-tip")).toContain(long);
    await act(async () => { fireEvent.click(releaseIcon()); });
    expect(reviewed.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-card-confirm-prompt").textContent).toBe(long);
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(reviewed.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: "7" }), "question/base/rel", expect.objectContaining({ actionToken: "tok-question/base/rel" }));
    view.unmount();

    const short = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([agentWritten("Use the release branch.", "agent")]) } });
    const shortView = render(<OverviewWithCardPane runtime={short} />);
    fireEvent.click(screen.getByTestId("agent-card-details"));
    expect(releaseIcon().getAttribute("data-tip")).toContain("Use the release branch.");
    await act(async () => { fireEvent.click(releaseIcon()); });
    expect(short.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.getByTestId("agent-card-confirm-prompt").textContent).toBe("Use the release branch.");
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(short.invokeCardAction).toHaveBeenCalledTimes(1);
    shortView.unmount();

    for (const prompt of ["Use the release branch.", long]) {
      const sent = runtime({ agentCards: { [FIXTURE_DAEMON_ID]: snapshot([agentWritten(prompt, "catalog")]) } });
      const shown = render(<OverviewWithCardPane runtime={sent} />);
      fireEvent.click(screen.getByTestId("agent-card-details"));
      expect(releaseIcon().getAttribute("data-tip")).toContain(prompt);
      await act(async () => { fireEvent.click(releaseIcon()); });
      expect(sent.invokeCardAction).toHaveBeenCalledTimes(1);
      expect(screen.queryByTestId("agent-card-confirm")).toBeNull();
      shown.unmount();
    }
  });

  it("shows none of it when the runtime offers no cards", () => {
    window.localStorage.setItem(OVERVIEW_MODE_STORAGE_KEY, "cards");
    render(<AgentOverview runtime={runtime({ agentCards: undefined })} onNavigate={vi.fn()} />);
    expect(screen.queryByTestId("overview-mode-switch")).toBeNull();
    expect(cardsViewShown()).toBe(false);
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

describe("a card whose agent id was reused (PR #2491 review, Qodo 5)", () => {
  /// Scenario: the daemon restarted and numbered a new agent 7, while coder's card from the earlier daemon process is still on screen. Clicking that card opens the agent it was read from, under that process, not the newer agent that took the same id; once the cards and the agent are from one process, the click opens the listed agent as before.
  it("opens the card's own agent, not a newer one with the same id", () => {
    const navigate = vi.fn();
    const live = runtime();
    const restarted = live.fleet!.map((deck) => ({ ...deck, agents: deck.agents.map((agent) => ({ ...agent, incarnation: "daemon-instance-2" })) }));
    const view = render(<AgentOverview runtime={{ ...live, fleet: restarted, snapshot: restarted[0] }} onNavigate={navigate} />);
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    const coder = () => document.querySelector(`article[data-card-key="${CODER.key}"]`) as HTMLElement;
    fireEvent.click(coder());
    expect(navigate).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "agent", deckId: FIXTURE_DAEMON_ID, agentId: "7", incarnation: INSTANCE }));
    navigate.mockClear();
    const current = { [FIXTURE_DAEMON_ID]: { ...snapshot([{ ...CODER, instance_id: "daemon-instance-2" }, TESTER, LONE]), instanceId: "daemon-instance-2" } };
    view.rerender(<AgentOverview runtime={{ ...live, fleet: restarted, snapshot: restarted[0], agentCards: current }} onNavigate={navigate} />);
    fireEvent.click(coder());
    expect(navigate).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "agent", deckId: FIXTURE_DAEMON_ID, agentId: "7", incarnation: "daemon-instance-2" }));
  });
});
