import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createFixtureFleet, createFixtureStartedAgent, FIXTURE_DAEMON_ID } from "../data/fixture";
import { DEFAULT_DESKTOP_FEATURES, type AgentCardAction, type AgentCardEntry, type AgentCardsSnapshot, type DeckFleet, type DeckRuntimeState } from "../types";
import { AgentOverview, agentKey } from "./AgentOverview";
import { OverviewWithCardPane } from "../test/cardPane";
import { cardsViewShown, dashboardMore, needsYouOnlyChecked, pressNeedsYouOnly } from "../test/needsYouMenu";
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
/** PR #2491 — the open agent pane's card: a panel beside the terminal from 2026-11-25 (it replaced the card overlay), its Card tab since 2026-11-26. */
const panel = () => screen.getByTestId("agent-card-panel");
const pane = () => screen.getByTestId("test-agent-pane");

function cardsView(live: DeckRuntimeState = runtime(), onNavigate = vi.fn()) {
  window.localStorage.setItem(MODE_KEY, "cards");
  return render(<OverviewWithCardPane runtime={live} onNavigate={onNavigate} />);
}

beforeEach(() => window.localStorage.clear());
afterEach(() => {
  vi.restoreAllMocks();
  window.localStorage.clear();
});

describe("desktop/agent-cards — the dashboard renders what the daemon sent (PRD #2374)", () => {
  /// Scenario: open the Cards view on a list from a daemon that builds no tree, whose entries the daemon grouped by dispatcher and orchestration. Every card is drawn as a root in the daemon's order — no grouping by `group.id` is rebuilt — and the top bar's ⋯ counts the cards the daemon marked as needing the user; there is no line above the cards (PR #2491 decision 2026-11-25).
  it("desktop/agent-cards/001: an untreed list draws every card as a root in the daemon's order, with the daemon's counts", () => {
    cardsView();
    expect(screen.queryByTestId("agent-cards-group")).toBeNull();
    expect([...document.querySelectorAll("article[data-card-key][data-depth='0']")].map((node) => node.getAttribute("data-card-key"))).toEqual(ORDER.map((card) => card.key));
    expect(screen.getByTestId("overview-needs-you-count").textContent).toBe("3");
    expect(dashboardMore()).toHaveAccessibleDescription("3 agents need you");
    expect(document.querySelector(".agent-cards-toolbar")).toBeNull();
    expect(screen.queryByText(/5 agents · 3 need you/)).toBeNull();
  });

  /// Scenario: show the Cards view for entries in an order that is neither alphabetical nor needs-you-first, then re-render with the daemon's new order. Each card shows the daemon's status words and needs-you marker even where they contradict the agent's own report, the cards follow each list's order, what the agent said is the Card tab's rather than the card's, and Needs you only fades every card the daemon did not mark, in place.
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
    // What the agent said and its fields are the Card tab's (issue #2418), not the compact card's.
    expect(within(cardOf(ZULU.key)).queryByText("Reported working, flagged by the deck anyway")).toBeNull();
    expect(within(cardOf(ZULU.key)).queryByText("half way")).toBeNull();
    expect(within(cardOf(ZULU.key)).queryByText("DETAIL-ONLY-TEXT")).toBeNull();

    // A new list from the daemon reorders the cards; nothing is re-sorted here.
    const reordered = [BRAVO, YANKEE, ALPHA, ZULU, MIKE].map((card) => ({ ...card, group: TEAM }));
    view.rerender(<AgentOverview runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot(reordered, 8) } }} onNavigate={vi.fn()} />);
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);

    expect(needsYouOnlyChecked()).toBe(false);
    pressNeedsYouOnly();
    expect(needsYouOnlyChecked()).toBe(true);
    // Every card stays in its place; the ones the daemon did not mark fade.
    expect(shownNames()).toEqual(["bravo", "yankee", "alpha", "zulu", "mike"]);
    expect(cardNodes().filter((node) => node.getAttribute("data-faded") === "true").map((node) => node.getAttribute("data-card-key"))).toEqual([BRAVO.key, ALPHA.key]);
    expect(screen.getByTestId("overview-needs-you-count").textContent).toBe("3");
    pressNeedsYouOnly();
    expect(cardNodes().filter((node) => node.getAttribute("data-faded") === "true")).toEqual([]);
  });

  /// Scenario: read a card with a primary action and press its icon-only Close control. The primary action is an icon on the card whose exact text is only in its hover, never on the card; Close opens the Close confirmation, a small dialog, which sends the card's Close token confirmed, while a daemon-disabled Close stays disabled and sends nothing. The agent's Card tab does not repeat Close: the pane's own Close agent does that.
  it("desktop/agent-cards/003: the compact card's Close sits beside its action icons, with its confirmation, token and disabled states", async () => {
    const close = action("close", { label: "Close", kind: "close", prompt: null, source: "deck", placement: "detail", confirm: true, confirm_detail: "This removes the worktree.", token: "close-round-two" });
    const card = { ...ZULU, actions: [...ZULU.actions.filter((a) => a.id !== "close"), close] };
    const live = runtime({}, [card]);
    const view = cardsView(live);
    // PRD #2374 decision 2026-11-24 (2): the primary action is an icon on the card, its text in its hover only.
    const merge = within(cardOf(card.key)).getByRole("button", { name: "Merge it" });
    expect(merge.textContent).toBe("");
    expect(merge.getAttribute("data-tip")).toContain(LONG_PROMPT);
    expect(within(cardOf(card.key)).queryByText(LONG_PROMPT)).toBeNull();
    const closeButton = within(cardOf(card.key)).getByRole("button", { name: /^Close(?: zulu(?:'s agent)?)?$/ });
    expect(closeButton.textContent).toBe("");
    expect(closeButton.title).toMatch(/Close/);
    closeButton.focus();
    expect(document.activeElement).toBe(closeButton);
    fireEvent.click(closeButton, { detail: 0 });
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    // Close does not open the agent's pane.
    expect(screen.queryByTestId("test-agent-pane")).toBeNull();
    const confirmation = screen.getByRole("alertdialog", { name: "Close?" });
    expect(confirmation).toHaveTextContent("This closes zulu.");
    expect(confirmation).toHaveTextContent("This removes the worktree.");
    await act(async () => { fireEvent.click(within(confirmation).getByRole("button", { name: "Close" })); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(
      { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: card.pane_id, agentId: card.agent_id },
      "close", expect.objectContaining({ confirm: true, actionToken: close.token }),
    );
    // After Send the confirmation says what became of it and closes on Close.
    expect(within(confirmation).getByTestId("agent-card-action-result")).toBeInTheDocument();
    fireEvent.click(within(confirmation).getByTestId("agent-card-confirm-close"));
    expect(screen.queryByRole("alertdialog")).toBeNull();

    // The Card tab does not repeat the deck's Close: the pane's header has Close agent.
    fireEvent.click(cardOf(card.key));
    expect(within(panel()).queryByRole("button", { name: /^Close/ })).toBeNull();
    view.unmount();

    const disabled = { ...card, actions: [action("close", { ...close, enabled: false, disabled_reason: "agent_stopped" })] };
    const stopped = runtime({}, [disabled]);
    cardsView(stopped);
    const disabledClose = within(cardOf(card.key)).getByRole("button", { name: /^Close(?: zulu(?:'s agent)?)?$/ });
    expect(disabledClose).toBeDisabled();
    expect(disabledClose.title).toContain("the agent is not running");
    fireEvent.click(disabledClose);
    expect(stopped.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.queryByRole("alertdialog")).toBeNull();
    expect(screen.queryByTestId("test-agent-pane")).toBeNull();
  });

  /// Scenario: open an agent from its table row, then switch to Cards and press its name, then its status line, then its body. Each opens the same agent as the row, through the shared open-agent action, on the same view as the row — its pane on Terminal, as every way into an agent opens (PR #2491 decision 2026-11-26); none of them nests one control in another.
  it("desktop/agent-cards/005: a card's name and body open the same agent as its table row, on Terminal", () => {
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
    navigate.mockClear();
    const details = within(card).getByRole("button", { name: /agent working: open zulu/i });
    details.focus();
    expect(document.activeElement).toBe(details);
    fireEvent.click(details, { detail: 0 });
    expect(navigate).toHaveBeenCalledExactlyOnceWith(rowView);
    navigate.mockClear();
    fireEvent.click(card);
    expect(navigate).toHaveBeenCalledExactlyOnceWith(rowView);
    expect(screen.queryByRole("dialog", { name: "zulu" })).toBeNull();
    expect(card.querySelector("button button, button a, a button, [role='button'] button, [role='button'] a")).toBeNull();
  });

  /// Scenario: open the agent of a card whose agent reported a summary and an open question with two options. Its Card tab shows, under "agent says", the summary before the question and its two answer buttons, the fields, and, under "deck checked", what was sent from the card; it has no reply box (a reply is typed into the terminal), and an answer is pressed from the panel.
  it("desktop/agent-cards/004: the Card tab shows the summary, the question with its answers and what was sent, and no reply box", async () => {
    const card: AgentCardEntry = { ...ZULU, recent_deliveries: [{ action_id: "merge", label: "Merge it", text: "Earlier reply", token: "prior-token", outcome: "sent", message_id: "m-prior", state: "delivered", detail: null, at: "2026-11-17T00:00:00Z" }], questions: [{ id: "base", text: "Which base branch?", source: "agent", answered: null, options: [
      { id: "main", label: "Use main", action_id: "question/base/main" },
      { id: "release", label: "Use release", action_id: "question/base/release" },
    ] }], actions: [...ZULU.actions, action("question/base/main", { label: "Use main", prompt_source: "catalog" }), action("question/base/release", { label: "Use release" })] };
    const live = runtime({}, [card]);
    cardsView(live);
    fireEvent.click(within(cardOf(card.key)).getByTestId("agent-card-details"));
    const says = within(panel()).getByRole("region", { name: "What the agent says" });
    const summary = within(says).getByText(ZULU.report!.summary!);
    const question = within(says).getByText("Which base branch?");
    const main = within(says).getByRole("button", { name: "Use main" });
    within(says).getByRole("button", { name: "Use release" });
    const before = (a: HTMLElement, b: HTMLElement) => expect(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
    before(summary, question);
    before(question, main);
    expect(within(says).getByText("DETAIL-ONLY-TEXT")).toBeInTheDocument();
    expect(within(within(panel()).getByRole("region", { name: "Checked by the deck" })).getByRole("region", { name: "Sent from this card" })).toHaveTextContent("Earlier reply");
    expect(within(panel()).queryByRole("textbox")).toBeNull();
    expect(within(panel()).queryByRole("button", { name: /Send reply|^Reply$|^Open$/ })).toBeNull();
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    await act(async () => { fireEvent.click(main); });
    expect(live.invokeCardAction).toHaveBeenCalledWith(expect.objectContaining({ agentId: ZULU.agent_id }), "question/base/main", expect.objectContaining({ actionToken: "tok-question/base/main" }));
    expect(within(panel()).getByTestId("agent-card-action-result")).toHaveTextContent("Use main");
  });

  /// Scenario: open alpha's agent from its card, then move with the pane's previous, next and Next needing you. It walks the daemon's display order (wrapping), Next needing you lands only on daemon-marked agents, each move opens that agent's pane with its Card tab showing its actions as icons with their full prompt in the hover, why it needs you, and the agent-says / deck-checked labels; a new list re-renders the shown card in place, and a list without its agent says the card is not available right now while the moves still work.
  it("desktop/agent-cards/004: the pane moves between agents in display order, its Card tab updates live, and survives its agent closing", () => {
    const live = runtime();
    const view = cardsView(live);
    const shownAgent = () => pane().getAttribute("data-agent-id");
    fireEvent.click(within(cardOf(ALPHA.key)).getByTestId("agent-card-details"));
    expect(panel().getAttribute("data-card-key")).toBe(ALPHA.key);
    expect(within(panel()).queryByTestId("agent-card-panel-needs-you")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    expect(shownAgent()).toBe(MIKE.agent_id);
    expect(panel().getAttribute("data-card-key")).toBe(MIKE.key);
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(ZULU.key);
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(YANKEE.key);
    // Next that needs you, wrapping: yankee → zulu → mike → yankee, skipping alpha and bravo.
    fireEvent.click(screen.getByRole("button", { name: "Next agent that needs you" }));
    expect(panel().getAttribute("data-card-key")).toBe(ZULU.key);
    fireEvent.click(screen.getByRole("button", { name: "Next agent that needs you" }));
    expect(panel().getAttribute("data-card-key")).toBe(MIKE.key);
    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(BRAVO.key);
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(MIKE.key);

    // Everything on the entry, the daemon's needs-you reasons included.
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    fireEvent.click(screen.getByRole("button", { name: "Previous agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(ZULU.key);
    expect(within(panel()).getByTestId("agent-card-panel-needs-you").textContent).toContain("done");
    expect(within(panel()).getAllByTestId("agent-card-action-icon").map((icon) => icon.getAttribute("data-tip"))).toEqual([expect.stringContaining("Explain what you did."), expect.stringContaining(LONG_PROMPT)]);
    expect(within(panel()).getByText("DETAIL-ONLY-TEXT")).toBeInTheDocument();
    expect(within(panel()).getAllByText("agent says").length).toBeGreaterThan(0);
    expect(within(panel()).getAllByText("deck checked").length).toBeGreaterThan(0);

    // Live: the same key with a new revision re-renders in place.
    const updated = { ...ZULU, revision: 9, status: "error", report: { ...ZULU.report!, summary: "LIVE-UPDATED-SUMMARY" } };
    view.rerender(<OverviewWithCardPane runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([updated, ALPHA, MIKE, BRAVO, YANKEE], 9) } }} onNavigate={vi.fn()} />);
    expect(panel().getAttribute("data-card-key")).toBe(ZULU.key);
    expect(within(panel()).getByText("LIVE-UPDATED-SUMMARY")).toBeInTheDocument();
    expect(within(panel()).getByText("error")).toBeInTheDocument();

    // The shown agent is gone from the list.
    view.rerender(<OverviewWithCardPane runtime={{ ...live, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([ALPHA, MIKE, BRAVO, YANKEE], 10) } }} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("agent-card-gone").textContent).toContain("This card is not available right now.");
    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(ALPHA.key);
  });

  /// Scenario: stay in the Table view. Only the rows whose entry the daemon marked needs-you carry the marker, whatever their status; a row's ⋯ menu holds Show card and the daemon's primary action (none when the primary is `open`). Show card opens that agent's pane on Terminal, as every way into an agent does, from which the pane's next moves in the daemon's order, and clicking the row itself still opens the agent.
  it("desktop/agent-cards/005: table rows carry the daemon's needs-you marker and a menu that shows the card", () => {
    const navigate = vi.fn();
    render(<OverviewWithCardPane runtime={runtime()} onNavigate={navigate} />);
    const row = (card: AgentCardEntry) => screen.getByTestId(`overview-agent-${agentKey({ daemonId: FIXTURE_DAEMON_ID, id: card.agent_id })}`);
    for (const marked of [ZULU, MIKE, YANKEE]) expect(within(row(marked)).getByTestId("overview-needs-you")).toBeInTheDocument();
    for (const unmarked of [ALPHA, BRAVO]) expect(within(row(unmarked)).queryByTestId("overview-needs-you")).toBeNull();

    fireEvent.click(within(row(ZULU)).getByTestId("overview-row-menu"));
    expect(navigate).not.toHaveBeenCalled();
    expect(within(row(ZULU)).getByTestId("overview-row-primary").textContent).toBe("Merge it");
    fireEvent.click(within(row(ZULU)).getByTestId("overview-row-show-card"));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ kind: "agent", agentId: ZULU.agent_id }));
    expect(navigate.mock.lastCall?.[0]).not.toHaveProperty("cardTab");
    expect(panel().getAttribute("data-card-key")).toBe(ZULU.key);
    // The pane walks the daemon's order from the table too.
    fireEvent.click(screen.getByRole("button", { name: "Next agent" }));
    expect(panel().getAttribute("data-card-key")).toBe(ALPHA.key);

    fireEvent.click(within(row(YANKEE)).getByTestId("overview-row-menu"));
    expect(within(row(YANKEE)).getByTestId("overview-row-show-card")).toBeInTheDocument();
    expect(within(row(YANKEE)).queryByTestId("overview-row-primary")).toBeNull();
    fireEvent.keyDown(document, { key: "Escape" });

    navigate.mockClear();
    fireEvent.click(row(BRAVO).querySelector("td:last-child") ?? row(BRAVO));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ kind: "agent", agentId: BRAVO.agent_id }));
    expect(navigate.mock.calls[0][0]).not.toHaveProperty("cardTab");
  });

  /// Scenario: switch to Cards, unmount and render the dashboard again, then make the window's storage throw on read and on write. The choice is stored as `cards` under `dot-agent-deck.overview.mode` and comes back on the next render; a storage that throws on read shows the Table without an error; one that throws on write still switches for this session.
  it("desktop/agent-cards/006: the view is remembered per client and a broken storage falls back to the table", () => {
    const first = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
    expect(cardsViewShown()).toBe(false);
    fireEvent.click(screen.getByTestId("overview-mode-cards"));
    expect(window.localStorage.getItem(MODE_KEY)).toBe("cards");
    expect(cardsViewShown()).toBe(true);
    first.unmount();

    const second = render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-cards").getAttribute("aria-checked")).toBe("true");
    expect(cardsViewShown()).toBe(true);
    fireEvent.click(screen.getByTestId("overview-mode-table"));
    expect(window.localStorage.getItem(MODE_KEY)).toBe("table");
    second.unmount();

    window.localStorage.setItem(MODE_KEY, "cards");
    const getItem = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("storage blocked"); });
    const setItem = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("storage blocked"); });
    render(<AgentOverview runtime={runtime()} onNavigate={vi.fn()} />);
    expect(screen.getByTestId("overview-mode-table").getAttribute("aria-checked")).toBe("true");
    expect(cardsViewShown()).toBe(false);
    expect(() => fireEvent.click(screen.getByTestId("overview-mode-cards"))).not.toThrow();
    expect(cardsViewShown()).toBe(true);
    getItem.mockRestore();
    setItem.mockRestore();
  });

  /// Scenario: render the dashboard with a runtime that offers no cards while the window remembers the Cards view. No Table | Cards switch, cards grid, needs-you marker, row menu or Card tab appears, nothing asks the runtime to consume cards, and the table rows still open their agents; rendered with a runtime that offers cards, it consumes them once, on mount.
  it("desktop/agent-cards/007: with no cards offered none of the new controls render and no cards are consumed", () => {
    window.localStorage.setItem(MODE_KEY, "cards");
    const navigate = vi.fn();
    const consumeOff = vi.fn();
    const { unmount } = render(<AgentOverview runtime={runtime({ agentCards: undefined, consumeAgentCards: consumeOff })} onNavigate={navigate} />);
    expect(screen.queryByTestId("overview-mode-switch")).toBeNull();
    expect(screen.queryByRole("radiogroup", { name: "Dashboard view" })).toBeNull();
    expect(cardsViewShown()).toBe(false);
    expect(screen.queryByRole("button", { name: "More" })).toBeNull();
    expect(screen.queryByTestId("needs-you-chip")).toBeNull();
    expect(screen.queryByTestId("daemon-more")).toBeNull();
    expect(screen.queryAllByTestId("overview-needs-you")).toHaveLength(0);
    expect(screen.queryAllByTestId("overview-row-menu")).toHaveLength(0);
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();
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

/*
 * PRD #2374 decision 2026-11-24 (2), issue #2399 on PR #2491 — a card shows an
 * icon for each prompt action its agent offers right now (beside Close when
 * written; in the card's bottom row since issue #2399 review 2). TDD
 * (RED): written before the card draws them.
 *
 * The contract these hold the implementation to:
 * - one icon button per prompt action the card's Card tab lists, in the
 *   daemon's order, but never Reply (or any text-input action), Open, an
 *   answer to the agent's own question, or the deck's Close / Close keeping
 *   the worktree (Close keeps its own icon);
 * - each is `data-testid="agent-card-action-icon"` with `data-action` (the
 *   action id) and `data-icon` (the catalog icon it draws, or `generic` for no
 *   icon or one the app does not bundle), has no visible text, is named by the
 *   action's label, and its hover (`title`) says the label and the exact text;
 * - a press takes the Card tab's path: a catalog action is sent through the one
 *   `invokeCardAction`, a `confirm` action and text the agent wrote open the
 *   single-action review first, and a disabled one says why;
 * - at most {@link CARD_ICON_LIMIT} icons; the rest are one `+N` control
 *   (`agent-card-actions-more`) that opens the agent's pane on its Card tab, at its actions.
 */
const CARD_ICON_LIMIT = 3;

const iconsOn = (key: string) => within(cardOf(key)).queryAllByTestId("agent-card-action-icon");

function treeSnapshot(entries: AgentCardEntry[]): AgentCardsSnapshot {
  return { ...snapshot(entries), tree: true };
}

describe("desktop/agent-cards — action icons on a card (PRD #2374, 2026-11-24)", () => {
  /// Scenario: read a card whose agent offers two catalog prompt actions, an answer to its own question, Reply, Open, Close and Close keeping the worktree. The card shows one icon per prompt action, in the daemon's order, with Close, each named by its label with its label and exact text in its hover and no text on the card; a root's card and, once the root is opened, its child's card both do.
  it("desktop/agent-cards/046: a card shows an icon for each prompt action it offers, with Close, and the exact text on hover", () => {
    const question = { id: "base", text: "Which base branch?", source: "agent", options: [{ id: "main", label: "main", action_id: "question/base/main" }], answered: null };
    const root = entry("41", "root", {
      questions: [question],
      actions: [
        action("explain", { label: "Explain", prompt: "Explain what you did.", icon: null }),
        action("question/base/main", { label: "main", prompt: "Answer: main", prompt_source: "agent" }),
        action("merge", { label: "Merge it", prompt: LONG_PROMPT, icon: "git-merge", prompt_source: "catalog" }),
        ...deckActions(),
        action("close-keep-worktree", { label: "Close, keep worktree", kind: "close_keep_worktree", prompt: null, source: "deck", placement: "detail", confirm: true }),
      ],
      rollup: { total: 1, worst: "idle", counts: { idle: 1 }, needs_you: 0 },
    });
    const child = entry("42", "child", { parent: root.key, actions: [action("go-ahead", { label: "Go ahead", prompt: "Go ahead.", icon: "play" }), ...deckActions()] });
    cardsView(runtime({ agentCards: { [FIXTURE_DAEMON_ID]: treeSnapshot([root, child]) } }, [root, child]));

    const icons = iconsOn(root.key);
    expect(icons.map((icon) => icon.getAttribute("data-action"))).toEqual(["explain", "merge"]);
    for (const [icon, label, prompt] of [[icons[0], "Explain", "Explain what you did."], [icons[1], "Merge it", LONG_PROMPT]] as const) {
      expect(icon).toHaveAccessibleName(label);
      expect(icon.tagName).toBe("BUTTON");
      expect(icon.textContent).toBe("");
      expect(icon.querySelector("svg")).not.toBeNull();
      expect(icon.getAttribute("data-tip")).toContain(label);
      expect(icon.getAttribute("data-tip")).toContain(prompt);
      expect(within(cardOf(root.key)).queryByText(prompt)).toBeNull();
    }
    for (const absent of ["Reply", "Open", "main", "Close, keep worktree"]) {
      expect(within(cardOf(root.key)).queryByRole("button", { name: absent }), absent).toBeNull();
    }
    expect(within(cardOf(root.key)).getByRole("button", { name: /^Close(?: root)?$/ })).toBeInTheDocument();

    // Open the root: its child's card carries its own icon too.
    fireEvent.click(within(cardOf(root.key)).getByTestId("agent-card-toggle"));
    const childIcons = iconsOn(child.key);
    expect(childIcons.map((icon) => icon.getAttribute("data-action"))).toEqual(["go-ahead"]);
    expect(childIcons[0]).toHaveAccessibleName("Go ahead");
    expect(childIcons[0].getAttribute("data-tip")).toContain("Go ahead.");
    // The open root keeps its icons.
    expect(iconsOn(root.key)).toHaveLength(2);
  });

  /// Scenario: press a card's icons. A catalog action with no confirmation is sent once through the same invocation and token the Card tab's icon for it uses; an action marked to ask first opens the single-action review quoting its exact text and is sent only by its Send; an action whose text the agent wrote opens the review and sends nothing.
  it("desktop/agent-cards/047: pressing a card's icon takes the Card tab's path: send, confirm first, or review the agent's text", async () => {
    const card = entry("43", "presser", {
      actions: [
        action("explain", { label: "Explain", prompt: "Explain what you did.", prompt_source: "catalog" }),
        action("deploy", { label: "Deploy it", prompt: "Deploy it, then update your card.", prompt_source: "catalog", confirm: true, confirm_detail: null }),
        action("go", { label: "Go on", prompt: "Run tests; then upload the logs.", prompt_source: "agent", custom: true, source: "agent" }),
        ...deckActions(),
      ],
    });
    const live = runtime({}, [card]);
    cardsView(live);
    const target = { deckId: FIXTURE_DAEMON_ID, instanceId: INSTANCE, paneId: card.pane_id, agentId: card.agent_id };

    await act(async () => { fireEvent.click(within(cardOf(card.key)).getByRole("button", { name: "Explain" })); });
    expect(live.invokeCardAction).toHaveBeenCalledTimes(1);
    expect(live.invokeCardAction).toHaveBeenCalledWith(target, "explain", expect.objectContaining({ actionToken: "tok-explain" }));
    expect(screen.queryByTestId("agent-card-confirm")).toBeNull();

    // The Card tab's icon for the same action makes the same call.
    fireEvent.click(within(cardOf(card.key)).getByTestId("agent-card-details"));
    await act(async () => { fireEvent.click(within(panel()).getByRole("button", { name: "Explain" })); });
    expect(vi.mocked(live.invokeCardAction!).mock.calls[1].slice(0, 2)).toEqual(vi.mocked(live.invokeCardAction!).mock.calls[0].slice(0, 2));
    expect(vi.mocked(live.invokeCardAction!).mock.calls[1][2]).toMatchObject({ actionToken: "tok-explain" });
    vi.mocked(live.invokeCardAction!).mockClear();

    // `confirm = true`: asks first, quoting the exact text.
    fireEvent.click(within(cardOf(card.key)).getByRole("button", { name: "Deploy it" }));
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    const confirmation = screen.getByTestId("agent-card-confirm");
    expect(within(confirmation).getByTestId("agent-card-confirm-prompt").textContent).toBe("Deploy it, then update your card.");
    await act(async () => { fireEvent.click(within(confirmation).getByTestId("agent-card-confirm-send")); });
    expect(live.invokeCardAction).toHaveBeenCalledExactlyOnceWith(target, "deploy", expect.objectContaining({ confirm: true, actionToken: "tok-deploy" }));
    fireEvent.keyDown(screen.getByTestId("agent-card-confirm"), { key: "Escape" });
    expect(screen.queryByTestId("agent-card-confirm")).toBeNull();
    vi.mocked(live.invokeCardAction!).mockClear();

    // Text the agent wrote: the review step, nothing sent.
    fireEvent.click(within(cardOf(card.key)).getByRole("button", { name: "Go on" }));
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    expect(within(screen.getByTestId("agent-card-confirm")).getByTestId("agent-card-confirm-prompt").textContent).toBe("Run tests; then upload the logs.");
  });

  /// Scenario: read the card of a running agent whose prompt action the daemon disabled. Its icon reads as disabled yet stays reachable by keyboard, its hover names the action and says why in the Card tab's words, and pressing it sends nothing and opens nothing.
  it("desktop/agent-cards/048: a disabled action's icon is disabled and says why on hover", () => {
    const live = runtime({}, [MIKE]);
    cardsView(live);
    const ship = within(cardOf(MIKE.key)).getByRole("button", { name: "Ship it" });
    expect(ship).toHaveAttribute("data-testid", "agent-card-action-icon");
    expect(ship).toHaveAttribute("aria-disabled", "true");
    expect(ship, "a native disabled button cannot take keyboard focus").not.toBeDisabled();
    expect(ship.getAttribute("data-tip")).toContain("Ship it");
    expect(ship.getAttribute("data-tip")).toContain("Release it.");
    expect(ship.getAttribute("data-tip")).toContain("the agent is not running");
    fireEvent.click(ship);
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    expect(screen.queryByTestId("agent-card-panel")).toBeNull();
  });

  /// Scenario: read a card whose actions carry the catalog icons `git-merge` and `play`, no icon, and an icon name the app does not bundle. The two known icons are drawn as themselves; the action with no icon and the one with the unknown icon both get the one generic icon.
  it("desktop/agent-cards/049: an action with no icon, or one the app does not know, gets the generic icon", () => {
    const card = entry("44", "icons", {
      actions: [
        action("merge", { label: "Merge it", icon: "git-merge" }),
        action("go-ahead", { label: "Go ahead", icon: "play" }),
        action("plain", { label: "Plain", icon: null }),
        ...deckActions(),
      ],
    });
    const other = entry("45", "unknown-icon", { actions: [action("weird", { label: "Weird", icon: "no-such-icon-2399" }), ...deckActions()] });
    cardsView(runtime({}, [card, other]));
    expect(iconsOn(card.key).map((icon) => icon.getAttribute("data-icon"))).toEqual(["git-merge", "play", "generic"]);
    expect(iconsOn(other.key).map((icon) => icon.getAttribute("data-icon"))).toEqual(["generic"]);
    const svg = (icon: HTMLElement) => icon.querySelector("svg")?.outerHTML;
    expect(svg(iconsOn(other.key)[0])).toBe(svg(iconsOn(card.key)[2]));
    expect(svg(iconsOn(card.key)[0])).not.toBe(svg(iconsOn(card.key)[2]));
  });

  /// Scenario: read a card whose agent offers five prompt actions. The card shows the first three as icons, in the daemon's order, and a `+2` control instead of the rest; pressing it opens the agent's pane with its Card tab, whose icon row holds all five, and sends nothing.
  it("desktop/agent-cards/050: past three icons a card shows +N, which opens its agent's Card tab with every action", () => {
    const five = ["a1", "a2", "a3", "a4", "a5"].map((id) => action(id, { label: `Action ${id}`, prompt: `Do ${id}.` }));
    const card = entry("46", "busy", { actions: [...five, ...deckActions()] });
    const live = runtime({}, [card]);
    cardsView(live);
    expect(iconsOn(card.key).map((icon) => icon.getAttribute("data-action"))).toEqual(["a1", "a2", "a3"].slice(0, CARD_ICON_LIMIT));
    const more = within(cardOf(card.key)).getByTestId("agent-card-actions-more");
    expect(more).toHaveTextContent("+2");
    expect(more).toHaveAccessibleName(/2 more actions/);
    expect(within(cardOf(card.key)).getByRole("button", { name: /^Close(?: busy)?$/ })).toBeInTheDocument();
    fireEvent.click(more);
    expect(within(panel()).getAllByTestId("agent-card-action-icon").map((icon) => icon.getAttribute("data-action"))).toEqual(["a1", "a2", "a3", "a4", "a5"]);
    expect(live.invokeCardAction).not.toHaveBeenCalled();
  });
  /// Scenario: move keyboard focus onto a card's action icons: an ordinary one, one that asks first and one the daemon disabled on a running agent. Each shows a tooltip, which the icon names as its description, with the action's label and the exact text it sends — the confirm-first one saying it asks first, the disabled one saying why — and moving focus away hides it. Hovering the confirm-first and the disabled icons names the label too.
  it("desktop/agent-cards/063: keyboard focus on an action icon shows its label and exact text, disabled ones included", () => {
    const card = entry("47", "focused", {
      status: "thinking",
      actions: [
        action("explain", { label: "Explain", prompt: "Explain what you did.\nThen wait." }),
        action("deploy", { label: "Deploy it", prompt: "Deploy it, then update your card.", confirm: true }),
        action("ship", { label: "Ship it", prompt: "Release it.", enabled: false, disabled_reason: "agent_stopped" }),
        ...deckActions(),
      ],
    });
    cardsView(runtime({}, [card]));
    const cases = [
      { label: "Explain", prompt: "Explain what you did.\nThen wait.", note: undefined },
      { label: "Deploy it", prompt: "Deploy it, then update your card.", note: /confirm/i },
      { label: "Ship it", prompt: "Release it.", note: /the agent is not running/ },
    ];
    for (const { label, prompt, note } of cases) {
      const icon = within(cardOf(card.key)).getByRole("button", { name: label });
      expect(screen.queryByRole("tooltip")).toBeNull();
      act(() => icon.focus());
      expect(icon, `${label} takes keyboard focus`).toHaveFocus();
      const tip = screen.getByRole("tooltip");
      expect(tip).toHaveTextContent(label);
      expect(tip.textContent).toContain(prompt);
      if (note) expect(tip).toHaveTextContent(note);
      expect(icon).toHaveAccessibleDescription(expect.stringContaining(label));
      // Issue #2399 review 10: the description is always there, whether the tooltip is drawn or not.
      expect(document.getElementById(icon.getAttribute("aria-describedby") ?? "")?.textContent).toBe(tip.textContent);
      if (note) {
        expect(icon.getAttribute("data-tip")).toContain(label);
        expect(icon.getAttribute("data-tip")).toMatch(note);
      }
      act(() => icon.blur());
      expect(screen.queryByRole("tooltip")).toBeNull();
    }
  });

  /// Scenario: read cards offering exactly three and exactly four prompt actions: three draw three icons and no `+N`; four draw the first three and `+1`. Then the daemon sends the four-action card again with one action gone, a new token on the first, new text on the second and the third now disabled: the card redraws three icons with no `+N`, the new text and the disabled reason are in their hovers, and pressing the re-tokened icon sends the new token.
  it("desktop/agent-cards/064: the icon limit's boundary, and a card whose offered actions change redraws its icons", async () => {
    const three = entry("48", "three", { actions: [...["b1", "b2", "b3"].map((id) => action(id, { label: `Action ${id}` })), ...deckActions()] });
    const four = entry("49", "four", { actions: [...["c1", "c2", "c3", "c4"].map((id) => action(id, { label: `Action ${id}` })), ...deckActions()] });
    const live = runtime({}, [three, four]);
    const view = cardsView(live);
    expect(iconsOn(three.key).map((icon) => icon.getAttribute("data-action"))).toEqual(["b1", "b2", "b3"]);
    expect(within(cardOf(three.key)).queryByTestId("agent-card-actions-more")).toBeNull();
    expect(iconsOn(four.key).map((icon) => icon.getAttribute("data-action"))).toEqual(["c1", "c2", "c3"]);
    expect(within(cardOf(four.key)).getByTestId("agent-card-actions-more")).toHaveTextContent("+1");

    const changed = {
      ...four,
      revision: 2,
      actions: [
        action("c1", { label: "Action c1", prompt_source: "catalog", token: "tok-c1-new" }),
        action("c2", { label: "Action c2", prompt: "Do c2, now differently." }),
        action("c3", { label: "Action c3", enabled: false, disabled_reason: "agent_stopped" }),
        ...deckActions(),
      ],
    };
    const next = runtime({ invokeCardAction: live.invokeCardAction, agentCards: { [FIXTURE_DAEMON_ID]: snapshot([three, changed], 8) } }, [three, changed]);
    view.rerender(<AgentOverview runtime={next} onNavigate={vi.fn()} />);
    expect(iconsOn(four.key).map((icon) => icon.getAttribute("data-action"))).toEqual(["c1", "c2", "c3"]);
    expect(within(cardOf(four.key)).queryByTestId("agent-card-actions-more")).toBeNull();
    const c1 = within(cardOf(four.key)).getByRole("button", { name: "Action c1" });
    expect(within(cardOf(four.key)).getByRole("button", { name: "Action c2" }).getAttribute("data-tip")).toContain("Do c2, now differently.");
    const c3 = within(cardOf(four.key)).getByRole("button", { name: "Action c3" });
    expect(c3).toHaveAttribute("aria-disabled", "true");
    expect(c3.getAttribute("data-tip")).toContain("the agent is not running");
    await act(async () => { fireEvent.click(c1); });
    expect(next.invokeCardAction).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ paneId: four.pane_id }), "c1", expect.objectContaining({ actionToken: "tok-c1-new" }));
  });
});

describe("desktop/agent-cards — the card's action row, placement and long texts (issue #2399 review)", () => {
  /// Scenario: A card offers Explain (placement card), Notes (placement detail) and Ship (placement compact), with the tree on. The icons are drawn in the card's own bottom row — not in the header with Close, nor on the status line — Explain and Ship as icons and Notes not at all, while the agent's Card tab draws Notes as an icon too. The bottom row is drawn, empty, on a card with no icons too, so every card has the same rows.
  it("desktop/agent-cards/069: action icons sit in the card's bottom row, and a Card-panel-only action draws no icon on the card", async () => {
    const card = entry("60", "rower", {
      actions: [
        action("explain", { label: "Explain", prompt: "Explain what you did.", placement: "card" }),
        action("notes", { label: "Notes", prompt: "Write the release notes.", placement: "detail" }),
        action("ship", { label: "Ship", prompt: "Ship it.", placement: "compact" }),
        ...deckActions(),
      ],
    });
    const plain = entry("61", "plain", { actions: deckActions() });
    cardsView(runtime({ agentCards: { [FIXTURE_DAEMON_ID]: treeSnapshot([card, plain]) } }, [card, plain]));
    const foot = within(cardOf(card.key)).getByTestId("agent-card-foot");
    expect(within(foot).getAllByTestId("agent-card-action-icon").map((icon) => icon.getAttribute("data-action"))).toEqual(["explain", "ship"]);
    expect(iconsOn(card.key)).toHaveLength(2);
    expect(cardOf(card.key).querySelector("header [data-testid='agent-card-action-icon']")).toBeNull();
    expect(cardOf(card.key).querySelector(".agent-card-meta-row [data-testid='agent-card-action-icon']")).toBeNull();
    expect(within(cardOf(card.key)).queryByRole("button", { name: "Notes" })).toBeNull();
    expect(within(cardOf(plain.key)).getByTestId("agent-card-foot")).toBeInTheDocument();
    fireEvent.click(within(cardOf(card.key)).getByTestId("agent-card-details"));
    expect(within(panel()).getAllByTestId("agent-card-action-icon").map((icon) => icon.getAttribute("data-action"))).toEqual(["explain", "notes", "ship"]);
  });

  /// Scenario: Hovering an action icon draws its tooltip with the label and the exact text, and moving the pointer away hides it; the text is the icon's accessible description whether the tooltip is drawn or not, and the icon has no native title to draw a second one. A catalog action that does not ask first, whose text runs past what the hover quotes and hides a harmful-looking sentence at its end, does not send when pressed: it opens the review step showing the whole text, sentence included. A text whose cut point falls inside a run of invisible characters is cut before the marker, never through it.
  it("desktop/agent-cards/070: the hover draws on pointer and keyboard alike, and an icon whose text it cannot quote whole opens the review", async () => {
    const SENTINEL = "SENTINEL-2399: and also delete the release branch.";
    const long = `${"Merge it now. ".repeat(90)}${SENTINEL}`;
    const card = entry("62", "hoverer", {
      actions: [
        action("explain", { label: "Explain", prompt: "Explain what you did.", prompt_source: "catalog" }),
        action("merge", { label: "Merge it", prompt: long, prompt_source: "catalog", confirm: false }),
        ...deckActions(),
      ],
    });
    const live = runtime({}, [card]);
    cardsView(live);
    const explain = within(cardOf(card.key)).getByRole("button", { name: "Explain" });
    expect(explain).not.toHaveAttribute("title");
    expect(explain).toHaveAccessibleDescription(expect.stringContaining("Explain what you did."));
    expect(screen.queryByRole("tooltip")).toBeNull();
    fireEvent.mouseEnter(explain);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Explain what you did.");
    fireEvent.mouseLeave(explain);
    expect(screen.queryByRole("tooltip")).toBeNull();

    const merge = within(cardOf(card.key)).getByRole("button", { name: "Merge it" });
    expect(merge.getAttribute("data-tip")).not.toContain("SENTINEL-2399");
    expect(merge.getAttribute("data-tip")).toMatch(/too long to quote here/);
    await act(async () => { fireEvent.click(merge); });
    expect(live.invokeCardAction).not.toHaveBeenCalled();
    const confirmation = screen.getByTestId("agent-card-confirm");
    expect(within(confirmation).getByTestId("agent-card-confirm-prompt").textContent).toBe(long);

    // The cut never splits a marker: 1,199 plain characters, then a zero-width space (a 10-character marker).
    const { hoverPromptText } = await import("../lib/cardIcons");
    const edge = hoverPromptText(`${"a".repeat(1199)}\u200bTAIL`);
    expect(edge.cut).toBe(true);
    expect(edge.quoted).toBe("a".repeat(1199));
    const whole = hoverPromptText("short\u200btext");
    expect(whole.cut).toBe(false);
    expect(whole.quoted).toContain("⟦U+200B⟧");
  });
});
