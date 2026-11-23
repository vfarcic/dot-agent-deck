import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";
import { VOICE_ACTIONS } from "../lib/voiceActions";

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
  actions: [action("explain", { label: "Explain", prompt: "Explain what you did." }), action("merge", { label: "Merge it", prompt: LONG_PROMPT, prompt_source: "catalog" }), ...deckActions()],
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
  const agents = entries.map((card) => ({ ...createFixtureStartedAgent({ id: card.agent_id, daemonId: FIXTURE_DAEMON_ID, displayName: card.name, command: "claude", cwd: "/w" }), incarnation: INSTANCE }));
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
    desktopFeatures: { ...DEFAULT_DESKTOP_FEATURES },
    agentCards: { [FIXTURE_DAEMON_ID]: snapshot(entries) },
    invokeCardAction: vi.fn(async (_target, _actionId, options) => ({ request_id: options?.requestId ?? "r", outcome: "sent" as const, message_id: "m-contract-1" })),
    ...overrides,
  } as DeckRuntimeState;
}

/** The cards on screen, by the key the daemon gave each, in screen order. */
const cardNodes = () => [...document.querySelectorAll("article[data-card-key]")];
const shownKeys = () => cardNodes().map((node) => node.getAttribute("data-card-key"));
const shownNames = () => cardNodes().map((card) => card.getAttribute("aria-label")?.replace(/, needs you$/, ""));
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
  /// Scenario: open the Cards view on a list from a daemon that builds no tree, whose entries the daemon grouped by dispatcher and orchestration. Every card is drawn as a root in the daemon's order — no grouping by `group.id` is rebuilt — and the toolbar counts every card and every one the daemon marked.
  it("desktop/agent-cards/001: an untreed list draws every card as a root in the daemon's order, with the daemon's counts", () => {
    cardsView();
    expect(screen.queryByTestId("agent-cards-group")).toBeNull();
    expect([...document.querySelectorAll("article[data-card-key][data-depth='0']")].map((node) => node.getAttribute("data-card-key"))).toEqual(ORDER.map((card) => card.key));
    expect(within(screen.getByTestId("agent-cards")).getByText("5 agents · 3 need you")).toBeInTheDocument();
  });

  /// Scenario: show the Cards view for entries in an order that is neither alphabetical nor needs-you-first, then re-render with the daemon's new order. Each card shows the daemon's status words and needs-you marker even where they contradict the agent's own report, the cards follow each list's order, what the agent said is the overlay's rather than the card's, and Needs you only fades every card the daemon did not mark, in place.
  it("desktop/agent-cards/002: cards keep the daemon's order, status and needs-you, and Needs you only fades the rest", () => {
    const live = runtime();
    const view = cardsView(live);
    expect(shownNames()).toEqual(["zulu", "alpha", "mike", "bravo", "yankee"]);
    expect(shownKeys()).toEqual(ORDER.map((card) => card.key));

    // The deck-checked status, in the daemon's words as the table shows them, and its needs-you mark.
    expect(within(cardOf(ZULU.key)).getByTestId("agent-card-status").textContent).toBe("working");
    expect(within(cardOf(ZULU.key)).getByTestId("agent-card-needs-you")).toBeInTheDocument();
    expect(cardOf(ZULU.key).getAttribute("aria-label")).toBe("zulu, needs you");
    expect(within(cardOf(ALPHA.key)).getByTestId("agent-card-status").textContent).toBe("needs input");
    expect(within(cardOf(ALPHA.key)).queryByTestId("agent-card-needs-you")).toBeNull();
    expect(cardOf(ALPHA.key).getAttribute("aria-label")).toBe("alpha");
    expect(within(cardOf(MIKE.key)).getByTestId("agent-card-status").textContent).toBe("thinking");
    expect(within(cardOf(MIKE.key)).getByTestId("agent-card-needs-you")).toBeInTheDocument();
    expect(within(cardOf(YANKEE.key)).getByTestId("agent-card-status").textContent).toBe("stopped");
    // What the agent said and its fields are the overlay's (issue #2418), not the compact card's.
    expect(within(cardOf(ZULU.key)).queryByText("Reported working, flagged by the deck anyway")).toBeNull();
    expect(within(cardOf(ZULU.key)).queryByText("half way")).toBeNull();
    expect(within(cardOf(ZULU.key)).queryByText("DETAIL-ONLY-TEXT")).toBeNull();

    // A new list from the daemon reorders the cards; nothing is re-sorted here.
    const reordered = [BRAVO, YANKEE, ALPHA, ZULU, MIKE].map((card) => ({ ...card, group: TEAM }));
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot(reordered, 8) } }} onNavigate={vi.fn()} />);
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);

    const filter = screen.getByTestId("agent-cards-needs-you-filter");
    expect(filter.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(filter);
    expect(screen.getByTestId("agent-cards-needs-you-filter").getAttribute("aria-pressed")).toBe("true");
    // Every card stays in its place; the ones the daemon did not mark fade.
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);
    expect(cardNodes().filter((node) => node.getAttribute("data-faded") === "true").map((node) => node.getAttribute("data-card-key"))).toEqual([BRAVO.key, ALPHA.key]);
    expect(within(screen.getByTestId("agent-cards")).getByText("5 agents · 3 need you")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("agent-cards-needs-you-filter"));
    expect(cardNodes().filter((node) => node.getAttribute("data-faded") === "true")).toEqual([]);
  });

  /// Scenario: read a card with a primary action and press its icon-only Close control. The primary action is absent; Close shows the same confirmation and sends the same token as Close in the overlay, while a daemon-disabled Close stays disabled and sends nothing.
  it("desktop/agent-cards/003: the compact card keeps only Close, with the overlay's confirmation, token and disabled states", async () => {
    const close = action("close", { label: "Close", kind: "close", prompt: null, source: "deck", placement: "detail", confirm: true, confirm_detail: "This removes the worktree.", token: "close-round-two" });
    const card = { ...ZULU, actions: [...ZULU.actions.filter((a) => a.id !== "close"), close] };
    const live = runtime({}, [card]);
    const view = cardsView(live);
    expect(within(cardOf(card.key)).queryByRole("button", { name: "Merge it" })).toBeNull();
    expect(within(cardOf(card.key)).queryByText(LONG_PROMPT)).toBeNull();
    const closeButton = within(cardOf(card.key)).getByRole("button", { name: /^Close(?: zulu(?:'s agent)?)?$/ });
    expect(closeButton.textContent).toBe("");
    expect(closeButton.title).toMatch(/Close/);
    closeButton.focus();
    expect(document.activeElement).toBe(closeButton);
    fireEvent.click(closeButton, { detail: 0 });
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    const confirmation = screen.getByRole("alertdialog", { name: "Close?" });
    expect(confirmation).toHaveTextContent("This closes zulu.");
    expect(confirmation).toHaveTextContent("This removes the worktree.");
    await act(async () => { fireEvent.click(within(confirmation).getByRole("button", { name: "Close" })); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(
      { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: card.pane_id, agentId: card.agent_id },
      "close", expect.objectContaining({ confirm: true, actionToken: close.token }),
    );

    // The overlay's Close uses the same definition, rather than another close path.
    fireEvent.keyDown(overlay(), { key: "Escape" });
    fireEvent.click(cardOf(card.key));
    fireEvent.click(within(overlay()).getByTestId("agent-card-action-close"));
    await act(async () => { fireEvent.click(screen.getByTestId("agent-card-confirm-send")); });
    expect(vi.mocked(live.invokeCardAction!).mock.calls[1].slice(0, 2)).toEqual(vi.mocked(live.invokeCardAction!).mock.calls[0].slice(0, 2));
    expect(vi.mocked(live.invokeCardAction!).mock.calls[1][2]).toMatchObject({ confirm: true, actionToken: close.token });
    view.unmount();

    const disabled = { ...card, actions: [action("close", { ...close, enabled: false, disabled_reason: "agent_stopped" })] };
    const stopped = runtime({}, [disabled]);
    cardsView(stopped);
    const disabledClose = within(cardOf(card.key)).getByRole("button", { name: /^Close(?: zulu(?:'s agent)?)?$/ });
    expect(disabledClose).toBeDisabled();
    expect(disabledClose.title).toContain("the agent is not running");
    fireEvent.click(disabledClose);
    expect(stopped.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
  });

  /// Scenario: open an agent from its table row, then switch to Cards and press its name. Both open the same agent through the shared open-agent action; pressing the card body instead opens its overlay, with no agent navigation and no nested interactive controls.
  it("desktop/agent-cards/005: a card's name opens the same agent as its table row and its body opens only the overlay", () => {
    const navigate = vi.fn();
    const open = vi.spyOn(VOICE_ACTIONS.openAgent, "run");
    render(<AgentOverview runtime={runtime({}, [ZULU])} onNavigate={navigate} />);
    const row = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: ZULU.agent_id })}`);
    fireEvent.click(row.querySelector("td:last-child") ?? row);
    expect(navigate).toHaveBeenCalledTimes(1);
    const rowView = navigate.mock.calls[0][0];
    const rowTarget = open.mock.calls[0][1];
    navigate.mockClear();
    open.mockClear();
    fireEvent.click(screen.getByRole("radio", { name: "Cards" }));
    const card = cardOf(ZULU.key);
    const name = within(card).getByTestId("agent-card-open-agent");
    expect(name).toHaveAccessibleName(/zulu/);
    name.focus();
    expect(document.activeElement).toBe(name);
    fireEvent.click(name, { detail: 0 });
    expect(open).toHaveBeenCalledTimes(1);
    expect(open.mock.calls[0][1]).toEqual(rowTarget);
    expect(navigate).toHaveBeenCalledExactlyOnceWith(rowView);
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
    navigate.mockClear();
    const details = within(card).getByRole("button", { name: /agent working: show zulu's card/i });
    details.focus();
    expect(document.activeElement).toBe(details);
    fireEvent.click(details, { detail: 0 });
    expect(screen.getByRole("dialog", { name: "zulu" })).toBeInTheDocument();
    fireEvent.keyDown(overlay(), { key: "Escape" });
    fireEvent.click(card);
    expect(screen.getByRole("dialog", { name: "zulu" })).toBeInTheDocument();
    expect(navigate).not.toHaveBeenCalled();
    expect(card.querySelector("button button, button a, a button, [role='button'] button, [role='button'] a")).toBeNull();
  });

  /// Scenario: open a card whose agent reported a summary and an open question with two options. The reply field is already visible, and the summary, question/options and reply precede fields, other actions and deliveries; typing and sending uses the offered reply action.
  it("desktop/agent-cards/004: the overlay leads with summary, question, options and a directly visible reply field", async () => {
    const card: AgentCardEntry = { ...ZULU, recent_deliveries: [{ action_id: "merge", label: "Merge it", text: "Earlier reply", token: "prior-token", outcome: "sent", message_id: "m-prior", state: "delivered", detail: null, at: "2026-11-17T00:00:00Z" }], questions: [{ id: "base", text: "Which base branch?", source: "agent", answered: null, options: [
      { id: "main", label: "Use main", action_id: "question/base/main" },
      { id: "release", label: "Use release", action_id: "question/base/release" },
    ] }], actions: [...ZULU.actions, action("question/base/main", { label: "Use main" }), action("question/base/release", { label: "Use release" })] };
    const live = runtime({}, [card]);
    cardsView(live);
    fireEvent.click(within(cardOf(card.key)).getByTestId("agent-card-details"));
    const dialog = screen.getByRole("dialog", { name: "zulu" });
    const summary = within(dialog).getByText(ZULU.report!.summary!);
    const question = within(dialog).getByText("Which base branch?");
    const main = within(dialog).getByRole("button", { name: "Use main" });
    const release = within(dialog).getByRole("button", { name: "Use release" });
    const reply = within(dialog).getByRole("textbox", { name: "Reply to zulu" });
    const before = (a: HTMLElement, b: HTMLElement) => expect(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
    before(summary, question);
    before(question, main);
    before(main, reply);
    before(release, reply);
    before(reply, within(dialog).getByRole("region", { name: "Fields" }));
    before(reply, within(dialog).getByRole("region", { name: "Actions" }));
    before(reply, within(dialog).getByRole("region", { name: "Sent from this card" }));
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    fireEvent.change(reply, { target: { value: "Use the release branch." } });
    await act(async () => { fireEvent.click(within(dialog).getByRole("button", { name: "Send reply" })); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: ZULU.agent_id }), "reply", expect.objectContaining({ text: "Use the release branch." }));
  });

  /// Scenario: open a card's overlay and move with `]`, `[` and `n` and the buttons. It walks the daemon's display order (wrapping), `n` lands only on daemon-marked cards, the overlay shows every action with its full prompt and the agent-says / deck-checked labels, a new list re-renders the open card in place, a list without its key shows "This agent was closed." while navigation still works, and Escape closes it.
  it("desktop/agent-cards/004: the overlay navigates in display order, updates live, and survives its agent closing", () => {
    const live = runtime();
    const view = cardsView(live);
    fireEvent.click(within(cardOf(ALPHA.key)).getByTestId("agent-card-details"));
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
    expect(within(overlay()).getByText("error")).toBeInTheDocument();

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

  /// Scenario: render the dashboard with a runtime that offers no cards while the window remembers the Cards view. No Table | Cards switch, cards grid, needs-you marker, row menu or overlay appears, nothing asks the runtime to consume cards, and the table rows still open their agents; rendered with a runtime that offers cards, it consumes them once, on mount.
  it("desktop/agent-cards/007: with no cards offered none of the new controls render and no cards are consumed", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const navigate = vi.fn();
    const consumeOff = vi.fn();
    const { unmount } = render(<AgentOverview runtime={runtime({ agentCards: undefined, consumeAgentCards: consumeOff })} onNavigate={navigate} />);
    expect(screen.queryByTestId("overview-mode-switch")).toBeNull();
    expect(screen.queryByRole("radiogroup", { name: "Dashboard view" })).toBeNull();
    expect(screen.queryByTestId("agent-cards")).toBeNull();
    expect(screen.queryByTestId("agent-cards-needs-you-filter")).toBeNull();
    expect(screen.queryAllByTestId("overview-needs-you")).toHaveLength(0);
    expect(screen.queryAllByTestId("overview-row-menu")).toHaveLength(0);
    expect(screen.queryByTestId("agent-card-overlay")).toBeNull();
    // What keeps cards unsubscribed and unread is that nothing rendered asks
    // for them.
    expect(consumeOff).not.toHaveBeenCalled();
    const row = screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: ZULU.agent_id })}`);
    fireEvent.click(row.querySelector("td:last-child") ?? row);
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: ZULU.agent_id }));
    unmount();

    // Cards offered: the switch mounts and consumes, once across re-renders.
    const consumeOn = vi.fn();
    const live = runtime({ consumeAgentCards: consumeOn });
    const { rerender } = render(<AgentOverview runtime={live} onNavigate={navigate} />);
    expect(screen.getByTestId("overview-mode-switch")).toBeInTheDocument();
    expect(consumeOn).toHaveBeenCalledTimes(1);
    rerender(<AgentOverview runtime={live} onNavigate={navigate} />);
    expect(consumeOn).toHaveBeenCalledTimes(1);
  });
});
