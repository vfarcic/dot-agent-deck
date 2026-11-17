import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";

/*
 * PRD #2374 — the spec tests of the desktop half of the agent-cards contract
 * (`desktop/agent-cards/001`–`007`; `008` is in `AgentCardsVoice.test.tsx`).
 *
 * Written from the contract (§8 and its changes 12–18), not from the
 * components: each test renders the real dashboard against a daemon list and
 * asserts what the user sees. The fixtures deliberately CONTRADICT what a
 * client would work out for itself — a "working" agent the daemon says needs
 * the user, an idle-looking one it says does not, an action disabled on a
 * running agent, entries in neither alphabetical nor needs-you-first order,
 * a primary action that is not the first one — because the contract says the
 * desktop computes nothing about status, needs-you, enablement or order: a
 * view that recomputed any of them would fail here.
 *
 * The storage key `dot-agent-deck.overview.mode` is written out as the
 * contract names it rather than imported, so a renamed constant fails too.
 */

const MODE_KEY = "dot-agent-deck.overview.mode";
const INSTANCE = "daemon-instance-contract";

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

const deckActions = (): AgentCardAction[] => [
  action("reply", { label: "Reply", prompt: null, text_input: true, source: "deck", placement: "detail" }),
  action("open", { label: "Open", kind: "open", prompt: null, source: "deck", placement: "detail" }),
  action("close", { label: "Close", kind: "close", prompt: null, confirm: true, confirm_detail: null, source: "deck", placement: "detail" }),
];

function entry(agentId: string, name: string, overrides: Partial<AgentCardEntry> = {}): AgentCardEntry {
  return {
    key: `pane-${agentId}/${agentId}`,
    instance_id: INSTANCE,
    pane_id: `pane-${agentId}`,
    agent_id: agentId,
    agent_type: "claude_code",
    name,
    status: "idle",
    needs_you: false,
    needs_you_reasons: [],
    group: { id: "standalone", kind: "standalone", label: null },
    relations: {},
    report: null,
    fields: [],
    questions: [],
    actions: deckActions(),
    primary_action_id: "open",
    facts: { source: "deck", pull_request: null },
    revision: 1,
    updated_at: "2026-11-17T00:00:00Z",
    ...overrides,
  };
}

const DISPATCHER = { id: "dispatcher:p1/a1", kind: "dispatcher", label: "release dispatcher" };
const TEAM = { id: "orchestration:o-1", kind: "orchestration", label: "review team" };

const LONG_PROMPT = "Merge it, following this project's own merge procedure, then update your card with the merge commit.";

/*
 * zulu: status "working" — a client would say it does not need the user — but
 * the daemon says it does (reason `done`), and its primary is the SECOND
 * action, a long prompt.
 */
const ZULU = entry("31", "zulu", {
  status: "working",
  needs_you: true,
  needs_you_reasons: ["done"],
  group: DISPATCHER,
  report: { source: "agent", status: "working", summary: "Reported working, flagged by the deck anyway", published_at: "2026-11-17T00:00:00Z", revision: 4 },
  fields: [
    { name: "progress", label: "Progress", description: "What you are doing now.", type: "text", icon: "activity", placement: "card", custom: false, source: "agent", value: "half way" },
    { name: "details", label: "Details", description: "Anything else.", type: "long_text", icon: null, placement: "detail", custom: false, source: "agent", value: "DETAIL-ONLY-TEXT" },
  ],
  actions: [action("explain", { label: "Explain", prompt: "Explain what you did." }), action("merge", { label: "Merge it", prompt: LONG_PROMPT }), ...deckActions()],
  primary_action_id: "merge",
});
/*
 * alpha: "waiting_for_input" with a report saying "waiting" — a client would
 * say it needs the user — but the daemon says it does not.
 */
const ALPHA = entry("32", "alpha", {
  status: "waiting_for_input",
  needs_you: false,
  needs_you_reasons: [],
  group: DISPATCHER,
  report: { source: "agent", status: "waiting", summary: "Waiting, says the agent", published_at: "2026-11-17T00:00:00Z", revision: 1 },
  actions: [action("go-ahead", { label: "Go ahead", prompt: "Go ahead." }), ...deckActions()],
  primary_action_id: "go-ahead",
});
/*
 * mike: running ("thinking"), yet the daemon disabled its primary prompt
 * action, with a reason; and it needs the user for a reason no client
 * derives from its fields.
 */
const MIKE = entry("33", "mike", {
  status: "thinking",
  needs_you: true,
  needs_you_reasons: ["blocked"],
  group: TEAM,
  report: { source: "agent", status: "done", summary: "Done, but held", published_at: "2026-11-17T00:00:00Z", revision: 2 },
  actions: [action("ship", { label: "Ship it", prompt: "Release it.", enabled: false, disabled_reason: "agent_stopped" }), ...deckActions()],
  primary_action_id: "ship",
});
/* bravo: standalone, nothing reported, nothing needed. */
const BRAVO = entry("34", "bravo", { status: "idle" });
/*
 * yankee: in the dispatcher's group again, AFTER other groups — the daemon's
 * order puts it here, so a client that regrouped by id would merge it into
 * the first group.
 */
const YANKEE = entry("35", "yankee", {
  status: "stopped",
  needs_you: true,
  needs_you_reasons: ["stopped"],
  group: DISPATCHER,
  actions: [action("rerun", { label: "Re-run tests", prompt: "Re-run the tests.", confirm: true, confirm_detail: "Still pending: an unanswered question.", custom: true, source: "agent", enabled: false, disabled_reason: "agent_stopped" }), ...deckActions()],
  primary_action_id: "open",
});

const ORDER = [ZULU, ALPHA, MIKE, BRAVO, YANKEE];

function fleet(entries: AgentCardEntry[] = ORDER): DeckFleet {
  const deck = createFixtureFleet("fleet")[0];
  const agents = entries.map((card) => createFixtureStartedAgent({ id: card.agent_id, daemonId: FIXTURE_DAEMON_ID, displayName: card.name, command: "claude", cwd: "/w" }));
  return [{ ...deck, agents }];
}

function snapshot(entries: AgentCardEntry[], revision = 7): AgentCardsSnapshot {
  return { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, revision, entries, catalogErrors: [] };
}

function runtime(overrides: Partial<DeckRuntimeState> = {}, entries: AgentCardEntry[] = ORDER): DeckRuntimeState {
  const decks = fleet(entries);
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
    desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES, showAgentCards: true },
    agentCards: { [FIXTURE_DAEMON_ID]: snapshot(entries) },
    invokeCardAction: vi.fn(async (_target, _actionId, options) => ({ request_id: options?.requestId ?? "r", outcome: "sent" as const, message_id: "m-contract-1" })),
    ...overrides,
  } as DeckRuntimeState;
}

/** The cards on screen, by the key the daemon gave each, in screen order. */
const cardNodes = () => [...document.querySelectorAll("article[data-card-key]")];
const shownKeys = () => cardNodes().map((node) => node.getAttribute("data-card-key"));
const shownNames = () => screen.getAllByTestId("agent-card-show").map((button) => button.textContent);
const cardOf = (key: string) => {
  const found = cardNodes().find((node) => node.getAttribute("data-card-key") === key);
  if (!(found instanceof HTMLElement)) throw new Error(`no card ${key} on screen`);
  return found;
};
const overlay = () => screen.getByTestId("agent-card-overlay");

function cardsView(live: DeckRuntimeState = runtime()) {
  window.localStorage.setItem(MODE_KEY, "cards");
  return render(<AgentOverview runtime={live} onNavigate={vi.fn()} />);
}

beforeEach(() => window.localStorage.clear());
afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("desktop/agent-cards — the dashboard renders what the daemon sent (PRD #2374)", () => {
  /// Scenario: open the Cards view on a daemon list whose entries come in three groups, with the dispatcher's group appearing twice because the daemon ordered one of its members last. The grid shows one section per run of consecutive entries sharing a `group.id`, each headed by its label and "N agents · M need you", and the toolbar counts every card.
  it("desktop/agent-cards/001: the grid groups consecutive entries by group.id with the daemon's counts", () => {
    cardsView();
    const groups = screen.getAllByTestId("agent-cards-group");
    expect(groups.map((group) => group.getAttribute("aria-label"))).toEqual(["release dispatcher", "review team", "Not in a group", "release dispatcher"]);
    expect(groups.map((group) => group.getAttribute("data-group-kind"))).toEqual(["dispatcher", "orchestration", "standalone", "dispatcher"]);
    expect(groups.map((group) => within(group).getByTestId("agent-cards-group-count").textContent)).toEqual([
      "2 agents · 1 needs you",
      "1 agent · 1 needs you",
      "1 agent · 0 need you",
      "1 agent · 1 needs you",
    ]);
    expect(groups.map((group) => within(group).getAllByTestId("agent-card-show").map((button) => button.textContent))).toEqual([["zulu", "alpha"], ["mike"], ["bravo"], ["yankee"]]);
    expect(within(screen.getByTestId("agent-cards")).getByText("5 agents · 3 need you")).toBeInTheDocument();
  });

  /// Scenario: show the Cards view for entries in an order that is neither alphabetical nor needs-you-first, then re-render with the daemon's new order. Each card shows the daemon's status words and needs-you marker even where they contradict the agent's own report, the cards follow each list's order, and the Needs you filter keeps only the daemon-marked cards, in that order.
  it("desktop/agent-cards/002: cards keep the daemon's order, status and needs-you, and the filter hides the rest", () => {
    const live = runtime();
    const view = cardsView(live);
    expect(shownNames()).toEqual(["zulu", "alpha", "mike", "bravo", "yankee"]);
    expect(shownKeys()).toEqual(ORDER.map((card) => card.key));

    // The deck-checked status, as the daemon named it, and its needs-you mark.
    expect(within(cardOf(ZULU.key)).getByText("Working")).toBeInTheDocument();
    expect(within(cardOf(ZULU.key)).getByTestId("agent-card-needs-you")).toBeInTheDocument();
    expect(cardOf(ZULU.key).getAttribute("aria-label")).toBe("zulu, needs you");
    expect(within(cardOf(ALPHA.key)).getByText("Waiting for input")).toBeInTheDocument();
    expect(within(cardOf(ALPHA.key)).queryByTestId("agent-card-needs-you")).toBeNull();
    expect(cardOf(ALPHA.key).getAttribute("aria-label")).toBe("alpha");
    expect(within(cardOf(MIKE.key)).getByText("Thinking")).toBeInTheDocument();
    expect(within(cardOf(MIKE.key)).getByTestId("agent-card-needs-you")).toBeInTheDocument();
    expect(within(cardOf(YANKEE.key)).getByText("Stopped")).toBeInTheDocument();
    // The agent's summary, and a field placed `card`; a `detail` field is the overlay's only.
    expect(within(cardOf(ZULU.key)).getByText("Reported working, flagged by the deck anyway")).toBeInTheDocument();
    expect(within(cardOf(ZULU.key)).getByText("half way")).toBeInTheDocument();
    expect(within(cardOf(ZULU.key)).queryByText("DETAIL-ONLY-TEXT")).toBeNull();
    expect(within(cardOf(BRAVO.key)).getByText("No report from this agent yet.")).toBeInTheDocument();

    // A new list from the daemon reorders the cards; nothing is re-sorted here.
    const reordered = [BRAVO, YANKEE, ALPHA, ZULU, MIKE].map((card) => ({ ...card, group: TEAM }));
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot(reordered, 8) } }} onNavigate={vi.fn()} />);
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);

    const filter = screen.getByTestId("agent-cards-needs-you-filter");
    expect(filter.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(filter);
    expect(screen.getByTestId("agent-cards-needs-you-filter").getAttribute("aria-pressed")).toBe("true");
    expect(shownNames()).toEqual(["yankee", "zulu", "mike"]);
    // The counts stay the group's whole counts while the filter hides cards.
    expect(screen.getByTestId("agent-cards-group-count").textContent).toBe("5 agents · 3 need you");
    fireEvent.click(screen.getByTestId("agent-cards-needs-you-filter"));
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);
  });

  /// Scenario: press each card's primary button. The button is the action the daemon named primary (not the first one) and shows its label, with its prompt beneath cut to 60 characters and whole in the hover title; pressing it sends that entry's key and action id; a primary the daemon disabled on a running agent is a disabled button naming the reason and sends nothing; a primary that asks for confirmation opens the overlay's confirm step with the prompt and the pending detail, and sends only from that step, confirmed.
  it("desktop/agent-cards/003: the primary action sends the daemon's action, shows its prompt, and asks first when it must", async () => {
    const live = runtime();
    const first = cardsView(live);

    const zuluPrimary = within(cardOf(ZULU.key)).getByTestId("agent-card-primary");
    expect(zuluPrimary.textContent).toBe("Merge it");
    const preview = zuluPrimary.parentElement!.querySelector(".agent-card-prompt")!;
    expect(preview.textContent).toBe(`${LONG_PROMPT.slice(0, 59)}…`);
    expect(preview.textContent!.length).toBe(60);
    expect(preview.getAttribute("title")).toBe(LONG_PROMPT);
    await act(async () => { fireEvent.click(zuluPrimary); });
    expect(live.invokeCardAction).toHaveBeenCalledTimes(1);
    expect(live.invokeCardAction).toHaveBeenCalledWith(
      { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: "pane-31", agentId: "31" },
      "merge",
      expect.objectContaining({ requestId: expect.any(String) }),
    );
    expect(vi.mocked(live.invokeCardAction!).mock.calls[0][2]?.confirm).not.toBe(true);
    expect(within(cardOf(ZULU.key)).getByTestId("agent-card-result").textContent).toContain("m-contract-1");

    // Disabled by the daemon although the agent is running.
    const mikePrimary = within(cardOf(MIKE.key)).getByTestId("agent-card-primary");
    expect(mikePrimary.textContent).toBe("Ship it");
    expect(mikePrimary).toBeDisabled();
    expect(within(cardOf(MIKE.key)).getByText("the agent is not running")).toBeInTheDocument();
    fireEvent.click(mikePrimary);
    expect(live.invokeCardAction).toHaveBeenCalledTimes(1);

    // A confirm primary: the step first, nothing sent until it is confirmed.
    const confirmable = entry("36", "oscar", {
      status: "working",
      actions: [action("deploy", { label: "Deploy", prompt: "Deploy to staging.", confirm: true, confirm_detail: "Still pending: open pull request #12." }), ...deckActions()],
      primary_action_id: "deploy",
    });
    const second = runtime({}, [confirmable]);
    first.unmount();
    cardsView(second);
    fireEvent.click(within(cardOf(confirmable.key)).getByTestId("agent-card-primary"));
    expect(second.invokeCardAction).not.toHaveBeenCalled();
    expect(overlay().getAttribute("data-card-key")).toBe(confirmable.key);
    const step = screen.getByTestId("agent-card-confirm");
    expect(step.textContent).toContain("Deploy to staging.");
    expect(step.textContent).toContain("Still pending: open pull request #12.");
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(second.invokeCardAction).toHaveBeenCalledTimes(1);
    expect(second.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: "36", paneId: "pane-36" }), "deploy", expect.objectContaining({ confirm: true }));
  });

  /// Scenario: open a card's overlay and move with `]`, `[` and `n` and the buttons. It walks the daemon's display order (wrapping), `n` lands only on daemon-marked cards, the overlay shows every action with its full prompt and the agent-says / deck-checked labels, a new list re-renders the open card in place, a list without its key shows "This agent was closed." while navigation still works, and Escape closes it.
  it("desktop/agent-cards/004: the overlay navigates in display order, updates live, and survives its agent closing", () => {
    const live = runtime();
    const view = cardsView(live);
    fireEvent.click(within(cardOf(ALPHA.key)).getByTestId("agent-card-show"));
    expect(overlay().getAttribute("data-card-key")).toBe(ALPHA.key);
    expect(within(overlay()).getByRole("heading", { level: 2 }).textContent).toBe("alpha");
    expect(within(overlay()).getByText("2 of 5")).toBeInTheDocument();
    expect(within(overlay()).queryByTestId("agent-card-overlay-needs-you")).toBeNull();

    fireEvent.keyDown(overlay(), { key: "]" });
    expect(overlay().getAttribute("data-card-key")).toBe(MIKE.key);
    fireEvent.keyDown(overlay(), { key: "[" });
    fireEvent.keyDown(overlay(), { key: "[" });
    expect(overlay().getAttribute("data-card-key")).toBe(ZULU.key);
    fireEvent.keyDown(overlay(), { key: "[" });
    expect(overlay().getAttribute("data-card-key")).toBe(YANKEE.key);
    // Next that needs you, wrapping: yankee → zulu → mike → yankee, skipping alpha and bravo.
    fireEvent.keyDown(overlay(), { key: "n" });
    expect(overlay().getAttribute("data-card-key")).toBe(ZULU.key);
    fireEvent.click(screen.getByRole("button", { name: "Next card that needs you" }));
    expect(overlay().getAttribute("data-card-key")).toBe(MIKE.key);
    fireEvent.click(screen.getByRole("button", { name: "Next card" }));
    expect(overlay().getAttribute("data-card-key")).toBe(BRAVO.key);
    fireEvent.click(screen.getByRole("button", { name: "Previous card" }));
    expect(overlay().getAttribute("data-card-key")).toBe(MIKE.key);

    // Everything on the entry, the daemon's needs-you reasons included.
    fireEvent.keyDown(overlay(), { key: "[" });
    fireEvent.keyDown(overlay(), { key: "[" });
    expect(overlay().getAttribute("data-card-key")).toBe(ZULU.key);
    expect(within(overlay()).getByTestId("agent-card-overlay-needs-you").textContent).toContain("done");
    expect(within(overlay()).getAllByTestId("agent-card-action-prompt").map((node) => node.textContent)).toEqual(["Explain what you did.", LONG_PROMPT]);
    expect(within(overlay()).getByText("DETAIL-ONLY-TEXT")).toBeInTheDocument();
    expect(within(overlay()).getAllByText("agent says").length).toBeGreaterThan(0);
    expect(within(overlay()).getAllByText("deck checked").length).toBeGreaterThan(0);

    // Live: the same key with a new revision re-renders in place.
    const updated = { ...ZULU, revision: 9, status: "error", report: { ...ZULU.report!, summary: "LIVE-UPDATED-SUMMARY" } };
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([updated, ALPHA, MIKE, BRAVO, YANKEE], 9) } }} onNavigate={vi.fn()} />);
    expect(overlay().getAttribute("data-card-key")).toBe(ZULU.key);
    expect(within(overlay()).getByText("LIVE-UPDATED-SUMMARY")).toBeInTheDocument();
    expect(within(overlay()).getByText("Error")).toBeInTheDocument();

    // The open agent is gone from the list.
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([ALPHA, MIKE, BRAVO, YANKEE], 10) } }} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("agent-card-gone").textContent).toBe("This agent was closed.");
    expect(within(overlay()).getByRole("heading", { level: 2 }).textContent).toBe("zulu");
    fireEvent.keyDown(overlay(), { key: "]" });
    expect(overlay().getAttribute("data-card-key")).toBe(ALPHA.key);
    fireEvent.keyDown(overlay(), { key: "Escape" });
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
  });

  /// Scenario: stay in the Table view. Only the rows whose entry the daemon marked needs-you carry the marker, whatever their status; a row's ⋯ menu holds Show card and the daemon's primary action (none when the primary is `open`), Show card opens that entry's overlay, and clicking the row itself still opens the agent.
  it("desktop/agent-cards/005: table rows carry the daemon's needs-you marker and a menu that shows the card", () => {
    const navigate = vi.fn();
    render(<AgentOverview runtime={runtime()} onNavigate={navigate} />);
    const row = (card: AgentCardEntry) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: card.agent_id })}`);
    for (const marked of [ZULU, MIKE, YANKEE]) expect(within(row(marked)).getByTestId("overview-needs-you")).toBeInTheDocument();
    for (const unmarked of [ALPHA, BRAVO]) expect(within(row(unmarked)).queryByTestId("overview-needs-you")).toBeNull();

    fireEvent.click(within(row(ZULU)).getByTestId("overview-row-menu"));
    expect(navigate).not.toHaveBeenCalled();
    expect(within(row(ZULU)).getByTestId("overview-row-primary").textContent).toBe("Merge it");
    fireEvent.click(within(row(ZULU)).getByTestId("overview-row-show-card"));
    expect(overlay().getAttribute("data-card-key")).toBe(ZULU.key);
    // The overlay walks the daemon's order from the table too.
    fireEvent.keyDown(overlay(), { key: "]" });
    expect(overlay().getAttribute("data-card-key")).toBe(ALPHA.key);
    fireEvent.keyDown(overlay(), { key: "Escape" });

    fireEvent.click(within(row(YANKEE)).getByTestId("overview-row-menu"));
    expect(within(row(YANKEE)).getByTestId("overview-row-show-card")).toBeInTheDocument();
    expect(within(row(YANKEE)).queryByTestId("overview-row-primary")).toBeNull();
    fireEvent.keyDown(document, { key: "Escape" });

    fireEvent.click(row(BRAVO).querySelector("td:last-child") ?? row(BRAVO));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: BRAVO.agent_id }));
  });

  /// Scenario: switch to Cards, unmount and render the dashboard again, then make the window's storage throw on read and on write. The choice is stored as `cards` under `dot-agent-deck.overview.mode` and comes back on the next render; a storage that throws on read shows the Table without an error; one that throws on write still switches for this session.
  it("desktop/agent-cards/006: the view is remembered per client and a broken storage falls back to the table", () => {
    const first = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
    expect(screen.queryByTestId("agent-cards")).toBeNull();
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    expect(window.localStorage.getItem(MODE_KEY)).toBe("cards");
    expect(screen.getByTestId("agent-cards")).toBeInTheDocument();
    first.unmount();

    const second = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-cards").getAttribute("aria-checked")).toBe("true");
    expect(screen.getByTestId("agent-cards")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("overview-mode-table"));
    expect(window.localStorage.getItem(MODE_KEY)).toBe("table");
    second.unmount();

    window.localStorage.setItem(MODE_KEY, "cards");
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("storage blocked"); });
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("storage blocked"); });
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
    expect(screen.queryByTestId("agent-cards")).toBeNull();
    expect(() => fireEvent.click(screen.getByTestId("overview-mode-cards"))).not.toThrow();
    expect(screen.getByTestId("agent-cards")).toBeInTheDocument();
    getItem.mockRestore();
    setItem.mockRestore();
  });

  /// Scenario: render the dashboard with the experimental flag off while the daemon sends cards and the window remembers the Cards view. No Table | Cards switch, cards grid, needs-you marker, row menu or overlay appears, and the table rows still open their agents.
  it("desktop/agent-cards/007: with showAgentCards off none of the new controls render", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const navigate = vi.fn();
    render(<AgentOverview runtime={runtime({ desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES, showAgentCards: false } })} onNavigate={navigate} />);
    expect(screen.queryByTestId("overview-mode-switch")).toBeNull();
    expect(screen.queryByRole("radiogroup", { name: "Dashboard view" })).toBeNull();
    expect(screen.queryByTestId("agent-cards")).toBeNull();
    expect(screen.queryByTestId("agent-cards-needs-you-filter")).toBeNull();
    expect(screen.queryAllByTestId("overview-needs-you")).toHaveLength(0);
    expect(screen.queryAllByTestId("overview-row-menu")).toHaveLength(0);
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
    const row = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: ZULU.agent_id })}`);
    fireEvent.click(row.querySelector("td:last-child") ?? row);
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: ZULU.agent_id }));
  });
});
